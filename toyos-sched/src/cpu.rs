//! The per-CPU machine — spec §6, §7, §8.3, §8.4.
//!
//! A CPU's scheduler state is a `!Sync` value reachable only through that
//! CPU's own pointer; there is no global runqueue array, because a `static` of
//! a `!Sync` type does not compile. Everything a remote CPU can do is post a
//! message into [`CpuHandle`] and ring its doorbell.
//!
//! Every entry is a [`SchedPass`]: a type-state that must be disposed exactly
//! once and can only end in [`SchedPass::finish`], which returns an [`Action`]
//! the driver executes. When the action is returned every borrow of `CpuSched`
//! has ended, so no guard can leak across the context switch and nothing
//! scheduler-related has anywhere to run after the switch resumes.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::marker::PhantomData;

use crate::fair::{Frontier, QUANTUM_NS};
use crate::hw::{CpuId, Hw, Nanos, TraceEvent, TraceKind};
use crate::mailbox::{
    Doorbell, Kick, MailboxConsumer, MailboxNode, MailboxProducer, PostSlot, PreemptGuard,
    Quiesced, SchedMsg, SleepArm, Urgency,
};
use crate::msg::Msg;
use crate::queue::RunQueue;
use crate::sync::{Arc, AtomicU32, Ordering};
#[cfg(feature = "check")]
use crate::sync::AtomicU64;
use crate::task::{
    BlockedTask, Claim, DeadTask, ReadyTask, RunningTask, SchedPayload, TaskKey, TaskShared,
    TaskState, TransitTask, WaitClass, WakeCause, WakeReason,
};
use crate::timer::{TimerApplied, TimerPlan};
use crate::waitq::{CommittedTicket, CurrentTask};

/// Permission to switch. Holds pointers into the stable Box-backed task
/// records (spec §5.1); constructed only by safe code in
/// [`SchedPass::finish`], consumed by the driver's `unsafe Hw::switch`.
///
/// The keys let a driver do its own bookkeeping (trace, invariant I11's
/// `ctx_saved` shadow) without dereferencing the pointers — which is what
/// keeps the simulator free of `unsafe`.
#[must_use]
pub struct RunToken<X: SchedPayload> {
    restore: *const X::Ctx,
    save: *mut X::Ctx,
    incoming: Option<TaskKey>,
    outgoing: Option<TaskKey>,
}

impl<X: SchedPayload> RunToken<X> {
    pub fn restore_ptr(&self) -> *const X::Ctx {
        self.restore
    }

    pub fn save_ptr(&self) -> *mut X::Ctx {
        self.save
    }

    /// The task being switched to; `None` is this CPU's idle context.
    pub fn incoming(&self) -> Option<TaskKey> {
        self.incoming
    }

    /// The task being switched away from; `None` is this CPU's idle context.
    pub fn outgoing(&self) -> Option<TaskKey> {
        self.outgoing
    }
}

/// Proof that halting is safe, assembled from two independently unforgeable
/// halves (spec §7.5, §8.4):
///
/// * [`Quiesced`] — SLEEPING was published *before* a mailbox-empty check
///   that came back empty, so any message that check missed rings the
///   doorbell afterwards and its producer sends the IPI.
/// * [`TimerApplied`] — the pass's timer plan reached the hardware, so a
///   pending deadline is armed.
///
/// `finish()` is the only place both exist at once, and it only reaches that
/// point with an empty run queue. "Halt with work queued" and "halt with a
/// deadline unarmed" are therefore not asserted against; they cannot be said.
#[must_use]
pub struct SleepToken {
    armed: Option<Nanos>,
}

impl SleepToken {
    fn new(_quiesced: Quiesced, timer: TimerApplied) -> Self {
        Self {
            armed: timer.armed(),
        }
    }

    /// What the timer is programmed to — the driver's `hlt` wakes on it.
    pub fn armed(&self) -> Option<Nanos> {
        self.armed
    }
}

/// What the driver must do when the pass ends.
#[must_use]
pub enum Action<X: SchedPayload> {
    Run(RunToken<X>),
    /// The pass decided not to switch; whatever was loaded stays loaded. Its
    /// own variant rather than a `Run` whose `restore` and `save` are the same
    /// context, which would make a self-switch representable.
    Resume,
    /// Nothing runnable, and this CPU is already on its idle context.
    Idle(SleepToken),
}

/// A parked task, plus the two facts that are only meaningful while parked.
///
/// The deadline lives *here* and nowhere else (spec §6.1, §8.3), so a task
/// that is not parked structurally cannot have one, and no second copy can
/// disagree with this one about what the CPU owes. The spec's `since` field is
/// omitted for the same reason: the residency stamp is in the task record.
pub struct ParkedEntry<X: SchedPayload> {
    task: BlockedTask<X>,
    deadline: Option<Nanos>,
    class: WaitClass,
}

/// One parked task as an outside reader sees it. The invariants want the key
/// and the deadline; a blocked-task dump wants the payload and how long the
/// park has lasted, and it is the only thing that can read them — a `CpuSched`
/// is reachable from its own CPU alone.
pub struct ParkedView<'a, X: SchedPayload> {
    key: TaskKey,
    entry: &'a ParkedEntry<X>,
}

impl<X: SchedPayload> ParkedView<'_, X> {
    pub fn key(&self) -> TaskKey {
        self.key
    }

    pub fn deadline(&self) -> Option<Nanos> {
        self.entry.deadline
    }

    pub fn class(&self) -> WaitClass {
        self.entry.class
    }

    /// When this park began.
    pub fn since(&self) -> Nanos {
        self.entry.task.since()
    }

    pub fn ext(&self) -> &X {
        self.entry.task.ext()
    }

    pub fn is_rt(&self) -> bool {
        self.entry.task.rt().is_rt()
    }

    pub fn shared_state(&self) -> TaskState {
        self.entry.task.shared().state()
    }
}

/// What context this CPU currently has loaded — the save target of the next
/// switch. Distinct from `running`, which is `None` between a park and the
/// switch that leaves the parked task's stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Loaded {
    Idle,
    Task(TaskKey),
}

/// One CPU's complete scheduler state. `!Sync` and `!Send`: it is reachable
/// only through the CPU that owns it.
pub struct CpuSched<X: SchedPayload> {
    id: CpuId,
    running: Option<RunningTask<X>>,
    rq: RunQueue<X>,
    parked: BTreeMap<TaskKey, ParkedEntry<X>>,
    /// The task that exited on this CPU, freed by the NEXT pass — a pass
    /// cannot free the stack it is running on.
    zombie: Option<DeadTask<X>>,
    mailbox: MailboxConsumer<Msg<X>>,
    /// This CPU's single reusable `StealRequest` node (spec §7.7). Its
    /// in-flight flag *is* the "a probe is already outstanding" answer — one
    /// mechanism for every node kind.
    steal_probe: MailboxNode<Msg<X>>,
    /// Thieves that asked this pass; answered in `finish` from surplus, after
    /// the pick, so answering can never hand away the task we were about to
    /// run.
    steal_requests: Vec<CpuId>,
    quantum_end: Nanos,
    loaded: Loaded,
    loaded_ctx: *mut X::Ctx,
    idle_ctx: Box<X::Ctx>,
    /// What the one-shot timer is programmed to. Bookkeeping for invariant T.
    armed: Option<Nanos>,
    /// Negative-gate escape hatch only; see [`CpuSched::set_park_keeps_lapsed_lend`].
    #[cfg(feature = "protocol-port")]
    park_keeps_lapsed_lend: bool,
    /// Negative-gate escape hatch only; see [`CpuSched::set_migrate_keeps_the_corpse`].
    #[cfg(feature = "protocol-port")]
    migrate_keeps_the_corpse: bool,
    _not_sync: PhantomData<*mut ()>,
}

impl<X: SchedPayload> CpuSched<X> {
    /// `idle_ctx` is the context this CPU runs on when it has nothing to do.
    /// Having one is what lets a pass free the previous zombie: an idle CPU is
    /// never standing on a dead task's stack.
    pub fn new(id: CpuId, mailbox: MailboxConsumer<Msg<X>>, idle_ctx: X::Ctx) -> Self {
        let mut idle_ctx = Box::new(idle_ctx);
        let loaded_ctx: *mut X::Ctx = &mut *idle_ctx;
        Self {
            id,
            running: None,
            rq: RunQueue::new(),
            parked: BTreeMap::new(),
            zombie: None,
            mailbox,
            steal_probe: MailboxNode::new(),
            steal_requests: Vec::new(),
            quantum_end: Nanos::ZERO,
            loaded: Loaded::Idle,
            loaded_ctx,
            idle_ctx,
            armed: None,
            #[cfg(feature = "protocol-port")]
            park_keeps_lapsed_lend: false,
            #[cfg(feature = "protocol-port")]
            migrate_keeps_the_corpse: false,
            _not_sync: PhantomData,
        }
    }

    pub fn id(&self) -> CpuId {
        self.id
    }

    pub fn running(&self) -> Option<&RunningTask<X>> {
        self.running.as_ref()
    }

    /// The handle `WaitQueue::prepare_wait` needs. Only the running task can
    /// be produced, so registering somebody else's task has no expression.
    pub fn current_task(&self) -> Option<CurrentTask<'_, Msg<X>>> {
        self.running
            .as_ref()
            .map(|t| CurrentTask::new(t.shared(), self.id))
    }

    pub fn rq(&self) -> &RunQueue<X> {
        &self.rq
    }

    pub fn parked(&self) -> impl Iterator<Item = ParkedView<'_, X>> + '_ {
        self.parked.iter().map(|(key, entry)| ParkedView { key: *key, entry })
    }

    pub fn parked_task(&self, key: TaskKey) -> Option<&BlockedTask<X>> {
        self.parked.get(&key).map(|e| &e.task)
    }

    pub fn zombie_key(&self) -> Option<TaskKey> {
        self.zombie.as_ref().map(|z| z.key())
    }

    /// What the one-shot timer is programmed to (invariant T / I3).
    pub fn armed(&self) -> Option<Nanos> {
        self.armed
    }

    pub fn quantum_end(&self) -> Nanos {
        self.quantum_end
    }

    /// Is this CPU on its idle context? Only then may it halt.
    pub fn is_idle(&self) -> bool {
        self.loaded == Loaded::Idle
    }

    pub fn mailbox_is_empty(&self) -> bool {
        self.mailbox.is_empty()
    }

    /// The number of ready tasks, republished to the handle every pass for
    /// spawn placement (spec §9.4).
    pub fn ready_len(&self) -> usize {
        self.rq.len()
    }

    /// Lend the running task an RT window (spec §8.5): the path for a client
    /// that was *not* blocked when its producer signalled, and so takes the
    /// boost at its own consume point instead of through a wake cause.
    pub fn boost_current(&mut self, until: Nanos) {
        if let Some(current) = self.running.as_mut() {
            current.boost(until);
        }
    }

    /// `SYS_RT_ENTER` on the running task — permanent RT, as opposed to the
    /// bounded window a waker lends. The privilege gate lives at the syscall
    /// layer (spec §9.2).
    pub fn set_current_rt(&mut self, permanent: bool) {
        if let Some(current) = self.running.as_mut() {
            current.set_permanent_rt(permanent);
        }
    }
}

/// Broken protocol shapes, reproduced for the simulator's negative gates
/// (spec §10.3). Behind a feature the kernel does not enable, so they are not
/// compiled into production at all.
///
/// `scenarios::old_steal_port` uses these two to re-create the pre-cutover
/// idle-loop steal: pop a ready task straight out of a sibling's queue, carry
/// it unlocked on the thief's own stack, install it later. Note what is *not*
/// offered — a state-word transition. That omission is the bug: the transit
/// window is invisible to a concurrent retire scan, so a task can run with its
/// address space already freed, and the invariant walk must catch it.
#[cfg(feature = "protocol-port")]
impl<X: SchedPayload> CpuSched<X> {
    pub fn steal_ready(&mut self) -> Option<ReadyTask<X>> {
        self.rq.pop_surplus()
    }

    pub fn install_stolen(&mut self, task: ReadyTask<X>) {
        let vruntime = task.share().runnable_vruntime().unwrap_or(0);
        self.rq.insert(vruntime, task);
    }

    /// Clear the borrowed window at a park only `if now >= until`, so a lend
    /// blocked on before it ran out survives the block — which with
    /// [`crate::task::RtState::arm`] re-arming at the next dispatch is a task
    /// holding inherited RT forever off one lend. Invariant I9 must catch it;
    /// `scenarios::old_park_kept_the_lend` is the gate that proves it does.
    pub fn set_park_keeps_lapsed_lend(&mut self, keep: bool) {
        self.park_keeps_lapsed_lend = keep;
    }

    /// Hand a killed ready task to another CPU instead of reaping it where it
    /// is — the balance path before [`CpuSched::hand_off`] checked the kill
    /// bit. It puts the task in `InTransit`, whose reap rides an
    /// `Urgency::Normal` adopt and therefore waits for the destination's next
    /// voluntary pass; the retirer's own bound is wall clock. Invariant I14
    /// must catch it; `scenarios::old_migrate_kept_the_corpse` is the gate that
    /// proves it does.
    pub fn set_migrate_keeps_the_corpse(&mut self, keep: bool) {
        self.migrate_keeps_the_corpse = keep;
    }

    /// Order the fair band by something other than spec §9.2's insertion
    /// sequence. Invariant I13 must catch what that does to a share's threads;
    /// `scenarios::sibling_storm`'s two gates are what prove it does.
    pub fn set_fair_order(&mut self, order: crate::queue::FairOrder) {
        self.rq.set_order(order);
    }
}

/// The environment a pass runs against. One value, threaded by reference, so
/// that a pass cannot be constructed without the pieces that make its effects
/// deliverable.
pub struct Env<'e, H: Hw, P: PreemptGuard> {
    pub hw: &'e H,
    pub cpus: &'e CpuHandles<Msg<H::Payload>>,
    pub frontier: &'e Frontier,
    /// The pass runs preempt-disabled (spec §6.2), which is also what its own
    /// mailbox pushes need (N3).
    pub preempt: &'e P,
    /// Whether an idle pass probes for work and a loaded pass answers probes
    /// (spec §7.7, §9.4's pull half). A field and not a `cfg` so that both
    /// settings stay compiled and simulatable.
    pub steal: bool,
}

impl<H: Hw, P: PreemptGuard> Clone for Env<'_, H, P> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<H: Hw, P: PreemptGuard> Copy for Env<'_, H, P> {}

impl<X: SchedPayload> CpuSched<X> {
    fn trace<H: Hw<Payload = X>, P: PreemptGuard>(
        &self,
        env: Env<'_, H, P>,
        now: Nanos,
        kind: TraceKind,
    ) {
        env.hw.trace(TraceEvent {
            ts: now,
            cpu: self.id,
            kind,
        });
    }

    /// Hand a dead task's payload back to the environment. The linear value is
    /// consumed here and nowhere else, so the address-space `Arc` inside it is
    /// released exactly once.
    fn release<H: Hw<Payload = X>, P: PreemptGuard>(&self, dead: DeadTask<X>, env: Env<'_, H, P>) {
        let (key, payload, acct) = dead.finalize();
        env.hw.release(key, payload, acct);
    }

    /// Every death goes through here — including the reap paths, not just the
    /// exit path (invariant I11).
    ///
    /// A task whose context is the one this CPU is *currently executing on*
    /// cannot be handed back yet: in the kernel that record owns the kernel
    /// stack under the running `rsp`. It becomes the zombie and is finalized
    /// by the next pass, which by then runs on another context.
    fn dispose_dead<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        dead: DeadTask<X>,
        env: Env<'_, H, P>,
    ) {
        if self.loaded == Loaded::Task(dead.key()) {
            assert!(
                self.zombie.replace(dead).is_none(),
                "two zombies on one CPU: the previous pass failed to finalize",
            );
            return;
        }
        self.release(dead, env);
    }

    /// Put a ready task in this CPU's queue, counting it into its share.
    fn enqueue<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        task: ReadyTask<X>,
        env: Env<'_, H, P>,
    ) {
        let vruntime = task.share().enter_runnable(env.frontier);
        self.rq.insert(vruntime, task);
    }

    /// Transfer a ready task to `dst` as an unconsumed `Adopt`. The caller has
    /// already settled the share refcount: a task taken out of this queue must
    /// leave it, a task that never entered must not.
    ///
    /// A killed task is reaped here instead. `InTransit` is the one state whose
    /// reap is not backed by an interrupt: the retire that carries
    /// `Urgency::Preempt` is consumed and dropped by a destination that gets it
    /// ahead of the adopt (`handle_retire`'s home-is-me arm), and the adopt
    /// behind it is `Urgency::Normal`, which by design kicks nobody. Reading the
    /// kill bit here is what stops a CPU putting a task it knows is dead into
    /// that state; what remains is a kill that lands *after* the adopt was
    /// posted, and that case always has a `Msg::Retire` aimed at the same CPU.
    fn hand_off<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        task: ReadyTask<X>,
        dst: CpuId,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        #[cfg(not(feature = "protocol-port"))]
        let migrate_anyway = false;
        #[cfg(feature = "protocol-port")]
        let migrate_anyway = self.migrate_keeps_the_corpse;
        if task.shared().kill_pending() && !migrate_anyway {
            let key = task.key();
            // No `leave_runnable`: settling the share is the caller's, stated
            // above, and it has already happened.
            let dead = task.reap(self.id, now);
            self.trace(env, now, TraceKind::Retire { task: key });
            self.dispose_dead(dead, env);
            return;
        }
        let key = task.key();
        let urgency = if task.is_rt() {
            Urgency::Preempt
        } else {
            Urgency::Normal
        };
        let transit = task.migrate(self.id, dst, now);
        self.trace(env, now, TraceKind::Migrate { task: key, to: dst });
        let handle = env.cpus.get(dst);
        if handle.post_owned(
            Msg::Adopt { task: transit },
            Msg::adopt_node,
            urgency,
            env.preempt,
        ) == Kick::Send
        {
            env.hw.kick(dst);
        }
    }

    /// A CPU that has published SLEEPING, for RT wake-forwarding (spec
    /// §7.4.4). Reading the doorbells is a heuristic: a CPU that woke up in
    /// the meantime simply gets an ordinary adopt.
    fn idle_sibling<H: Hw<Payload = X>, P: PreemptGuard>(
        &self,
        env: Env<'_, H, P>,
    ) -> Option<CpuId> {
        (0..env.cpus.len())
            .map(|i| CpuId(i as u32))
            .find(|&cpu| cpu != self.id && env.cpus.get(cpu).doorbell().sleeping())
    }

    /// Wake placement (spec §9.4): keep the task local — that is where its
    /// cache lines are — unless this CPU is already running RT and the task
    /// is too, in which case an idle sibling gets it rather than queueing RT
    /// behind RT.
    fn place<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        task: ReadyTask<X>,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        if task.is_rt() && self.running.as_ref().is_some_and(|r| r.is_rt()) {
            if let Some(dst) = self.idle_sibling(env) {
                self.hand_off(task, dst, env, now);
                return;
            }
        }
        self.enqueue(task, env);
    }

    fn handle_wake<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        key: TaskKey,
        cause: WakeCause,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        let Some(entry) = self.parked.remove(&key) else {
            // Not parked here any more: a `Retire` reaped it, or its deadline
            // fired first and this wake lost the arbitration CAS. Keys are
            // never reused, so a stale wake is provably about a task that is
            // no longer waiting — a benign no-op (spec §7.6).
            return;
        };
        let task = entry.task.wake(self.id, cause, entry.class, now);
        self.trace(env, now, TraceKind::Wake { task: key });
        self.place(task, env, now);
    }

    fn handle_adopt<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        task: TransitTask<X>,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        let key = task.key();
        match task.adopt(self.id, now) {
            Ok(ready) => {
                self.trace(env, now, TraceKind::Adopt { task: key });
                self.enqueue(ready, env);
            }
            // Killed while in flight. This arm is the whole termination
            // argument of the retire chase: whoever ends up owning the task
            // reaps it (spec §7.6).
            Err(dead) => {
                self.trace(env, now, TraceKind::Retire { task: key });
                self.dispose_dead(dead, env);
            }
        }
    }

    fn handle_retire<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        shared: &Arc<TaskShared<Msg<X>>>,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        let key = shared.key();
        if let Some(entry) = self.parked.remove(&key) {
            let dead = entry.task.reap(self.id, entry.class, now);
            self.trace(env, now, TraceKind::Retire { task: key });
            self.dispose_dead(dead, env);
            return;
        }
        if let Some(ready) = self.rq.remove(key) {
            ready.share().leave_runnable(env.frontier);
            let dead = ready.reap(self.id, now);
            self.trace(env, now, TraceKind::Retire { task: key });
            self.dispose_dead(dead, env);
            return;
        }
        if self.running.as_ref().is_some_and(|r| r.key() == key) {
            // A running task cannot be yanked out from under its own kernel
            // stack; it dies at its next safe point, bounded by the quantum
            // (spec §7.6). Consuming the message here is only sound because
            // the sticky kill bit outlives it and *every* safe point honours
            // it: the pick reaps a killed ready task, and `WaitTicket::commit`
            // refuses to park a killed one. Without that second arm a task that
            // parked through this window would never be picked again, so
            // nothing would ever reap it.
            env.hw.need_resched(self.id);
            return;
        }
        // Somewhere else. Re-post the *same* node — legal precisely because
        // this consumer just unlinked it — unless the word names this CPU,
        // which means an `Adopt` is on its way here: re-posting would spin
        // against the producer, and the sticky kill bit already guarantees
        // the adopter reaps it on arrival.
        match shared.state() {
            TaskState::Dead => {}
            state if home_of(state) == Some(self.id) => {}
            _ => {
                crate::retire::chase(shared, env.cpus, env.hw, env.preempt);
            }
        }
    }

    /// Consume the mailbox. Runs before anything else in a pass, so a woken
    /// RT task is in the RT band *before* the pick (spec §7.4).
    fn drain<H: Hw<Payload = X>, P: PreemptGuard>(&mut self, env: Env<'_, H, P>, now: Nanos) {
        while let Some(msg) = self.mailbox.pop(env.preempt) {
            match msg {
                Msg::Wake { key, cause } => self.handle_wake(key, cause, env, now),
                Msg::Adopt { task } => self.handle_adopt(task, env, now),
                Msg::StealRequest { thief } => self.steal_requests.push(thief),
                Msg::Retire { shared } => self.handle_retire(&shared, env, now),
            }
        }
    }

    /// The earliest deadline this CPU owes, and the only thing `apply_timer`
    /// arms from.
    ///
    /// Public because it is also the answer to *may this CPU start something
    /// long*: a CPU that owes a wake is the wrong one to run unbounded I/O on,
    /// and the idle loop asks before it flushes (`sched::driver::owes_deadline`).
    pub fn earliest_deadline(&self) -> Option<Nanos> {
        self.parked.values().filter_map(|entry| entry.deadline).min()
    }

    fn next_due(&self, now: Nanos) -> Option<TaskKey> {
        self.parked
            .iter()
            .find(|(_, entry)| entry.deadline.is_some_and(|at| at <= now))
            .map(|(key, _)| *key)
    }

    /// Fire every deadline that is due, arbitrating with remote wakers
    /// through the same claim CAS they use (spec §8.3).
    fn fire_deadlines<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        while let Some(key) = self.next_due(now) {
            let entry = self.parked.get_mut(&key).expect("just found");
            match entry.task.shared().claim_wake() {
                Claim::Parked(cpu) => {
                    assert_eq!(cpu, self.id, "a task parked here claims another CPU")
                }
                Claim::PrePark => panic!("a parked task cannot be pre-park"),
                // A remote waker got there first and its `Wake` is in flight;
                // no later claim can succeed either, so this timeout can never
                // fire and the entry stops claiming it will. Clearing it is
                // also what advances the loop — the deadline that will not be
                // honoured and the deadline that is not reported are one field.
                Claim::Lost => {
                    entry.deadline = None;
                    continue;
                }
            }
            let entry = self.parked.remove(&key).expect("still there");
            let task = entry.task.wake(
                self.id,
                WakeCause::new(WakeReason::Timeout),
                entry.class,
                now,
            );
            self.trace(env, now, TraceKind::TimerFire);
            self.place(task, env, now);
        }
    }
}

fn home_of(state: TaskState) -> Option<CpuId> {
    match state {
        TaskState::Running(cpu)
        | TaskState::Ready(cpu)
        | TaskState::Committing(cpu, _)
        | TaskState::Blocked(cpu)
        | TaskState::WakeQueued(cpu)
        | TaskState::InTransit(cpu) => Some(cpu),
        TaskState::Dead => None,
    }
}

/// How long one scheduler pass is modelled to take on the machine it runs on
/// (spec §10.2, §14's preempt-off risk). **Measured by a `feature = "check"`
/// build and gated in the harness against the measurement; asserted by
/// nothing.**
///
/// The number is the simulator's own modelling error made explicit. The sim
/// charges a pass **zero** time — every step it takes is either a workload op
/// or an interrupt — so invariant I4's RT wake-latency bound
/// (`IPI_LATENCY_NS + max KernelSection + 2 × RUN_CHUNK_NS`) omits the pass
/// entirely. A pass that costs more than the 200 µs the sim models for IPI
/// delivery would be the largest unmodelled term in that bound, so that is
/// where the budget sits: 2% of a quantum, and an order of magnitude above any
/// pass that is doing scheduling rather than work.
///
/// It is a *policy* number, like `MAX_USER_STR` and `MAX_FDS`: nothing in the
/// design forces 200 µs. If a measurement crosses it on honest work, the honest
/// response is to find out which pass grew and why — not to raise it.
///
/// **Why no panic stands over it.** The only clock either world can read across
/// a pass is wall clock — `rdtsc` in the kernel — and a guest's wall clock
/// advances while its vCPU is descheduled by the host. `elapsed` is therefore
/// the pass plus any interval the hypervisor took the CPU away, and the second
/// term is set by the host's scheduler, which this CPU neither observes nor
/// controls and no constant bounds. A panic may only assert what its own site
/// observes and what no workload scales, so the cost of a pass is recorded as a
/// distribution ([`PassCosts`]) and judged where composed quantities are judged:
/// in the harness and the simulator.
pub const MAX_PASS_NS: u64 = 200_000;

/// Power-of-two buckets a pass-cost histogram keeps. The top one saturates at
/// 2^30 ns ≈ 1.07 s, which is longer than any pass a machine survives.
#[cfg(feature = "check")]
pub const PASS_COST_BUCKETS: usize = 32;

/// Bucket 0 is exactly zero; bucket `b > 0` covers `[2^(b-1), 2^b)` ns.
#[cfg(feature = "check")]
pub fn pass_cost_bucket(ns: u64) -> usize {
    ((u64::BITS - ns.leading_zeros()) as usize).min(PASS_COST_BUCKETS - 1)
}

/// The exclusive upper bound of bucket `b`, and `u64::MAX` for the saturating
/// top one. A quantile is reported as one of these: "this fraction of passes
/// cost *less than* this many nanoseconds" is the strongest true statement a
/// histogram supports, and it is the statement the harness gates.
#[cfg(feature = "check")]
pub fn pass_cost_bucket_end(bucket: usize) -> u64 {
    if bucket >= PASS_COST_BUCKETS - 1 {
        u64::MAX
    } else {
        1u64 << bucket
    }
}

/// One CPU's pass-cost distribution, as a value: the wire form between the
/// kernel that measures and the harness that judges.
///
/// `over` is exact and the histogram is not, which is deliberate — it is the
/// direct successor of the quantity the removed assert panicked over, and
/// rounding it to a power of two would lose the one number a reader compares
/// against [`MAX_PASS_NS`] by eye.
#[cfg(feature = "check")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PassCostReport {
    pub cpu: CpuId,
    /// Passes measured since boot.
    pub count: u64,
    /// The longest single pass measured. **Includes any interval the host took
    /// the CPU away**, so it is printed and never gated on.
    pub max_ns: u64,
    /// Passes measured at more than [`MAX_PASS_NS`].
    pub over: u64,
    pub buckets: [u64; PASS_COST_BUCKETS],
}

#[cfg(feature = "check")]
impl PassCostReport {
    pub fn empty(cpu: CpuId) -> Self {
        Self {
            cpu,
            count: 0,
            max_ns: 0,
            over: 0,
            buckets: [0; PASS_COST_BUCKETS],
        }
    }

    /// The smallest bucket end below which `num/den` of all passes fall.
    ///
    /// Zero samples answer 0: a caller that gates on this must check
    /// [`Self::count`] first, and the harness does.
    pub fn quantile_upper_ns(&self, num: u64, den: u64) -> u64 {
        assert!(den > 0 && num <= den, "a quantile is num/den with num <= den");
        if self.count == 0 {
            return 0;
        }
        // Ceiling, so `num/den` is reached rather than approached: at
        // 999/1000 of 1000 samples the answer covers all 1000, not 999.
        let want = (self.count as u128 * num as u128).div_ceil(den as u128);
        let mut seen: u128 = 0;
        for (bucket, &n) in self.buckets.iter().enumerate() {
            seen += n as u128;
            if seen >= want {
                return pass_cost_bucket_end(bucket);
            }
        }
        u64::MAX
    }
}

/// The wire form. Parsed back by [`PassCostReport::parse`], and the two are
/// held together by a round-trip test rather than by care.
#[cfg(feature = "check")]
impl core::fmt::Display for PassCostReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "sched-check pass-costs cpu={} n={} max={} over={} b=",
            self.cpu.0, self.count, self.max_ns, self.over,
        )?;
        let mut first = true;
        for (bucket, &n) in self.buckets.iter().enumerate() {
            if n == 0 {
                continue;
            }
            if !first {
                write!(f, ",")?;
            }
            first = false;
            write!(f, "{bucket}:{n}")?;
        }
        if first {
            write!(f, "-")?;
        }
        Ok(())
    }
}

#[cfg(feature = "check")]
impl PassCostReport {
    /// The prefix a capture is searched for. One contiguous literal, because
    /// the build's artifact gate looks for exactly these bytes in the kernel
    /// image to prove the check build carries the instrument at all.
    pub const PREFIX: &'static str = "sched-check pass-costs cpu=";

    /// Read one report out of a console line. `None` for a line that is not
    /// one, or one whose fields do not parse — a malformed report is not a
    /// zeroed report, and a caller that treats it as one gates on nothing.
    pub fn parse(line: &str) -> Option<Self> {
        let body = &line[line.find(Self::PREFIX)? + Self::PREFIX.len()..];
        let mut fields = body.split_whitespace();
        let cpu: u32 = fields.next()?.parse().ok()?;
        let mut report = Self::empty(CpuId(cpu));
        report.count = fields.next()?.strip_prefix("n=")?.parse().ok()?;
        report.max_ns = fields.next()?.strip_prefix("max=")?.parse().ok()?;
        report.over = fields.next()?.strip_prefix("over=")?.parse().ok()?;
        let hist = fields.next()?.strip_prefix("b=")?;
        if hist != "-" {
            for pair in hist.split(',') {
                let (bucket, n) = pair.split_once(':')?;
                let bucket: usize = bucket.parse().ok()?;
                let n: u64 = n.parse().ok()?;
                *report.buckets.get_mut(bucket)? = n;
            }
        }
        // A histogram that does not add up to `n` is a truncated line or a
        // changed format, and either way the numbers below it mean nothing.
        (report.buckets.iter().sum::<u64>() == report.count).then_some(report)
    }
}

/// One CPU's live pass-cost recorder, written only by that CPU and read by
/// anyone (spec §10.2). Exists only in a `feature = "check"` build.
///
/// Plain relaxed load/store rather than read-modify-write: the writer is the
/// owning CPU inside its own pass, so there is no contention to lose to, and an
/// uncontended `lock xadd` on the pass path is the operation that costs most
/// under emulation.
#[cfg(feature = "check")]
pub struct PassCosts {
    count: AtomicU64,
    max_ns: AtomicU64,
    over: AtomicU64,
    buckets: [AtomicU64; PASS_COST_BUCKETS],
}

#[cfg(feature = "check")]
impl PassCosts {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            over: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; PASS_COST_BUCKETS],
        }
    }

    fn bump(cell: &AtomicU64) {
        cell.store(cell.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }

    fn record(&self, ns: u64) {
        Self::bump(&self.count);
        Self::bump(&self.buckets[pass_cost_bucket(ns)]);
        if ns > self.max_ns.load(Ordering::Relaxed) {
            self.max_ns.store(ns, Ordering::Relaxed);
        }
        if ns > MAX_PASS_NS {
            Self::bump(&self.over);
        }
    }

    /// Passes measured so far — the driver's cadence reads this and nothing
    /// else, so a report costs one load on the pass path between reports.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn report(&self, cpu: CpuId) -> PassCostReport {
        let mut report = PassCostReport::empty(cpu);
        report.max_ns = self.max_ns.load(Ordering::Relaxed);
        report.over = self.over.load(Ordering::Relaxed);
        for (bucket, cell) in self.buckets.iter().enumerate() {
            report.buckets[bucket] = cell.load(Ordering::Relaxed);
        }
        // Last, and from the buckets rather than the counter: a remote reader
        // can land between the two writes, and a report whose histogram is one
        // short of its `n` fails `parse`'s sum check downstream for no reason.
        report.count = report.buckets.iter().sum();
        report
    }
}

/// Pass type-states: a pass must be disposed exactly once, and disposal is
/// the only route to [`SchedPass::finish`].
pub enum Undisposed {}
pub enum Disposed {}

mod sealed {
    pub trait PassState {}
    impl PassState for super::Undisposed {}
    impl PassState for super::Disposed {}
}

pub use sealed::PassState;

/// The only way to touch a [`CpuSched`].
#[must_use = "a pass must be disposed and finished"]
pub struct SchedPass<'c, 'e, H: Hw, P: PreemptGuard, S: PassState> {
    cpu: &'c mut CpuSched<H::Payload>,
    env: Env<'e, H, P>,
    now: Nanos,
    _state: PhantomData<S>,
}

impl<'c, 'e, H: Hw, P: PreemptGuard> SchedPass<'c, 'e, H, P, Undisposed> {
    /// Enter the scheduler.
    ///
    /// `now` is sampled ONCE by the driver and threaded as a value. Re-reading
    /// the clock mid-pass is irreproducible in a simulator and skews a deadline
    /// against the arming computed from it.
    ///
    /// Entry order is load-bearing: clear the doorbell edge *before* draining
    /// (so a message posted after the drain re-raises it, §7.3), free the
    /// previous pass's zombie (we are not on its stack), charge the running
    /// task, then drain and fire deadlines — so that everything the pick can
    /// see is already visible.
    pub fn begin(cpu: &'c mut CpuSched<H::Payload>, env: Env<'e, H, P>, now: Nanos) -> Self {
        env.cpus.get(cpu.id).doorbell().begin_pass();
        if let Some(zombie) = cpu.zombie.take() {
            cpu.release(zombie, env);
        }
        if let Some(current) = cpu.running.as_mut() {
            let ns = current.charge(now);
            if ns > 0 {
                current
                    .share()
                    .charge(ns)
                    .expect("charging a share with no runnable threads");
            }
        }
        cpu.drain(env, now);
        cpu.fire_deadlines(env, now);
        Self {
            cpu,
            env,
            now,
            _state: PhantomData,
        }
    }

    pub fn cpu(&self) -> &CpuSched<H::Payload> {
        self.cpu
    }

    pub fn now(&self) -> Nanos {
        self.now
    }

    /// The current task keeps its claim on the CPU (subject to the preemption
    /// decision in `finish`).
    pub fn dispose_none(self) -> SchedPass<'c, 'e, H, P, Disposed> {
        self.dispose()
    }

    /// Voluntary yield, or a quantum the driver already decided to end.
    pub fn dispose_yield(self) -> SchedPass<'c, 'e, H, P, Disposed> {
        if let Some(current) = self.cpu.running.take() {
            let task = current.preempt(self.cpu.id, self.now);
            let vruntime = task
                .share()
                .runnable_vruntime()
                .expect("a yielding task's share must be runnable");
            self.cpu.rq.insert(vruntime, task);
        }
        self.dispose()
    }

    /// Park the current task **before** the switch. Sound only because of
    /// per-CPU ownership: a wake for the just-parked task arrives as a message
    /// to this same CPU and cannot be processed until the next pass, which
    /// necessarily runs after the switch completes. The stack-reuse race is
    /// sequentially impossible, not locked away (spec §6.2).
    pub fn dispose_block(
        self,
        ticket: CommittedTicket<Msg<H::Payload>>,
        deadline: Option<Nanos>,
    ) -> SchedPass<'c, 'e, H, P, Disposed> {
        let current = self
            .cpu
            .running
            .take()
            .expect("dispose_block without a running task");
        let key = current.key();
        let class = ticket.class();
        let task = current.park(
            &ticket,
            self.cpu.id,
            self.now,
            #[cfg(feature = "protocol-port")]
            self.cpu.park_keeps_lapsed_lend,
        );
        task.share().leave_runnable(self.env.frontier);
        self.cpu.parked.insert(
            key,
            ParkedEntry {
                task,
                deadline,
                class,
            },
        );
        self.cpu
            .trace(self.env, self.now, TraceKind::ParkCommit { task: key });
        self.dispose()
    }

    /// The current task exits. Its record survives as the zombie until the
    /// next pass, which runs on another stack.
    pub fn dispose_exit(self) -> SchedPass<'c, 'e, H, P, Disposed> {
        let current = self
            .cpu
            .running
            .take()
            .expect("dispose_exit without a running task");
        let key = current.key();
        let dead = current.die(self.cpu.id, self.now);
        dead.share().leave_runnable(self.env.frontier);
        self.cpu.dispose_dead(dead, self.env);
        self.cpu
            .trace(self.env, self.now, TraceKind::Retire { task: key });
        self.dispose()
    }

    fn dispose(self) -> SchedPass<'c, 'e, H, P, Disposed> {
        SchedPass {
            cpu: self.cpu,
            env: self.env,
            now: self.now,
            _state: PhantomData,
        }
    }
}

impl<H: Hw, P: PreemptGuard> SchedPass<'_, '_, H, P, Disposed> {
    /// The only exit. Picks the next task, answers steal requests from
    /// surplus, publishes load, and — LAST — programs the timer. Arming after
    /// every change to `parked` is the whole proof of invariant T (spec §8.4):
    /// with no window between the last change and the arming, "deadline exists
    /// but timer unarmed" is not a state the code can be in.
    pub fn finish(self) -> Action<<H as Hw>::Payload> {
        // Sampled before the pass is consumed, so the second clock read below
        // measures the pass and nothing else. `now` is threaded as a value
        // everywhere else in the core precisely so a decision cannot depend on
        // when it is read; this reads the clock again, which is why it exists
        // only in a check build and feeds a histogram rather than a decision.
        //
        // The handle is taken out here for the same reason `hw` is: by the time
        // the measurement lands, `finish_inner` has consumed the pass and with
        // it every borrow of the `CpuSched`.
        #[cfg(feature = "check")]
        let (hw, handle, entered) = (self.env.hw, self.env.cpus.get(self.cpu.id), self.now);
        let action = self.finish_inner();
        #[cfg(feature = "check")]
        handle.pass_costs().record(hw.now().since(entered));
        action
    }

    fn finish_inner(mut self) -> Action<<H as Hw>::Payload> {
        loop {
            self.preempt_if_due();
            self.pick();
            self.answer_steal_requests();
            self.env
                .cpus
                .get(self.cpu.id)
                .publish_load(self.cpu.rq.len() as u32);
            if self.cpu.running.is_some() {
                return self.switch_to_current();
            }
            if !self.cpu.is_idle() {
                return self.switch_to_idle();
            }
            match self.try_sleep() {
                Ok(action) => return action,
                // A message landed between the drain and the final check:
                // consume it and decide again (spec §7.5).
                Err(()) => continue,
            }
        }
    }

    /// Quantum expiry and RT preemption, the two reasons a running task loses
    /// the CPU without asking.
    fn preempt_if_due(&mut self) {
        let Some(current) = self.cpu.running.as_ref() else {
            return;
        };
        let due = self.now >= self.cpu.quantum_end || (self.cpu.rq.has_rt() && !current.is_rt());
        if !due {
            return;
        }
        let current = self.cpu.running.take().expect("checked above");
        let task = current.preempt(self.cpu.id, self.now);
        let vruntime = task
            .share()
            .runnable_vruntime()
            .expect("a preempted task's share must be runnable");
        self.cpu.rq.insert(vruntime, task);
    }

    fn pick(&mut self) {
        if self.cpu.running.is_some() {
            return;
        }
        // A lapsed borrowed window must not demote the task out of the RT band
        // here: queue time spends none of it, so `ReadyTask::dispatch` re-arms
        // it instead. The band stays whatever it was at insert.
        while let Some((vruntime, task)) = self.cpu.rq.pop_next() {
            if task.shared().kill_pending() {
                // Checked here rather than asserted absent in `dispatch`: a
                // remote CPU sets the bit at any instant, so an assert would be
                // a race. Reaping at the pick has no false positive.
                task.share().leave_runnable(self.env.frontier);
                let key = task.key();
                let dead = task.reap(self.cpu.id, self.now);
                self.cpu
                    .trace(self.env, self.now, TraceKind::Retire { task: key });
                self.cpu.dispose_dead(dead, self.env);
                continue;
            }
            self.env.frontier.advance(vruntime);
            let key = task.key();
            self.cpu.running = Some(task.dispatch(self.cpu.id, self.now));
            self.cpu.quantum_end = self.now.after(QUANTUM_NS);
            self.cpu
                .trace(self.env, self.now, TraceKind::Schedule { task: key });
            return;
        }
    }

    /// Answer probes from surplus only (`fair_len() > 1`), after the pick — so
    /// a CPU can never give away the task it was about to run.
    fn answer_steal_requests(&mut self) {
        if !self.env.steal {
            self.cpu.steal_requests.clear();
            return;
        }
        while let Some(thief) = self.cpu.steal_requests.pop() {
            if self.cpu.rq.fair_len() <= 1 {
                self.cpu.steal_requests.clear();
                return;
            }
            let Some(task) = self.cpu.rq.pop_surplus() else {
                return;
            };
            task.share().leave_runnable(self.env.frontier);
            self.cpu.hand_off(task, thief, self.env, self.now);
        }
    }

    fn apply_timer(&mut self) -> TimerApplied {
        let deadline = self.cpu.earliest_deadline();
        let quantum = self.cpu.running.as_ref().map(|_| self.cpu.quantum_end);
        let plan = TimerPlan::compute(quantum, deadline);
        match plan {
            TimerPlan::Arm(at) => self.env.hw.set_timer(at),
            TimerPlan::Stop => self.env.hw.stop_timer(),
        }
        self.cpu.armed = plan.armed();
        // Invariant T is a statement about a CPU *outside* a pass, and the
        // arming that makes it true is the last thing a pass does — so the
        // check cannot move earlier.
        #[cfg(feature = "check")]
        crate::invariants::check_cpu(self.cpu);
        TimerApplied::new(plan.armed())
    }

    fn switch_to_current(&mut self) -> Action<<H as Hw>::Payload> {
        self.apply_timer();
        let outgoing = match self.cpu.loaded {
            Loaded::Idle => None,
            Loaded::Task(key) => Some(key),
        };
        let current = self.cpu.running.as_mut().expect("checked by the caller");
        let incoming = current.key();
        if outgoing == Some(incoming) {
            return Action::Resume;
        }
        let restore = current.ctx_ptr();
        let save = self.cpu.loaded_ctx;
        self.cpu.loaded = Loaded::Task(incoming);
        self.cpu.loaded_ctx = restore;
        Action::Run(RunToken {
            restore,
            save,
            incoming: Some(incoming),
            outgoing,
        })
    }

    /// Nothing to run while a task's context is loaded: leave its stack for
    /// the CPU's idle context. Only then may the next pass halt — or free a
    /// zombie.
    fn switch_to_idle(&mut self) -> Action<<H as Hw>::Payload> {
        self.apply_timer();
        let outgoing = match self.cpu.loaded {
            Loaded::Idle => unreachable!("switch_to_idle while already idle"),
            Loaded::Task(key) => Some(key),
        };
        let restore: *mut <H::Payload as SchedPayload>::Ctx = &mut *self.cpu.idle_ctx;
        let save = self.cpu.loaded_ctx;
        self.cpu.loaded = Loaded::Idle;
        self.cpu.loaded_ctx = restore;
        self.cpu.trace(self.env, self.now, TraceKind::IdleEnter);
        Action::Run(RunToken {
            restore,
            save,
            incoming: None,
            outgoing,
        })
    }

    /// The idle disposition: ask the busiest CPU for work, then publish
    /// SLEEPING *before* the final mailbox check. `Err(())` means a message
    /// arrived in between — stay awake and decide again.
    fn try_sleep(&mut self) -> Result<Action<<H as Hw>::Payload>, ()> {
        self.post_steal_probe();
        let timer = self.apply_timer();
        let arm: SleepArm<'_> = self.env.cpus.get(self.cpu.id).doorbell().arm_sleep();
        match arm.confirm(&self.cpu.mailbox) {
            Ok(quiesced) => Ok(Action::Idle(SleepToken::new(quiesced, timer))),
            Err(_awake) => {
                self.env.cpus.get(self.cpu.id).doorbell().begin_pass();
                self.cpu.drain(self.env, self.now);
                self.cpu.fire_deadlines(self.env, self.now);
                Err(())
            }
        }
    }

    /// One probe at a time (spec §7.7): if the previous one is still in
    /// flight the claim fails and we simply do not post another — the
    /// outstanding probe will be answered, and this CPU sleeps with its
    /// doorbell armed.
    fn post_steal_probe(&mut self) {
        if !self.env.steal {
            return;
        }
        let Some((victim, _)) = (0..self.env.cpus.len())
            .map(|i| CpuId(i as u32))
            .filter(|&cpu| cpu != self.cpu.id)
            .map(|cpu| (cpu, self.env.cpus.get(cpu).load()))
            .max_by_key(|&(_, load)| load)
        else {
            return;
        };
        if self.env.cpus.get(victim).load() < 2 {
            return;
        }
        let Some(slot) = self.cpu.steal_probe.claim() else {
            return;
        };
        let thief = self.cpu.id;
        if self.env.cpus.get(victim).post(
            slot,
            Msg::StealRequest { thief },
            Urgency::Normal,
            self.env.preempt,
        ) == Kick::Send
        {
            self.env.hw.kick(victim);
        }
    }
}

/// The globally shared, `Sync` face of a CPU, and the whole remote surface:
/// post a message, ring the doorbell, read the published load (spec §6.1).
pub struct CpuHandle<M> {
    id: CpuId,
    post: MailboxProducer<M>,
    doorbell: Doorbell,
    /// Ready-count heuristic, published for spawn placement only.
    load: AtomicU32,
    /// The on-target counterpart to the simulator's invariants (spec §10.2):
    /// the sim asserts what a pass *does*, this measures what a pass *costs*.
    ///
    /// Everything else in `feature = "check"` is a statement about state the
    /// core owns, which is checkable in either world. Cost is not: the
    /// simulator's clock does not advance inside a step, so on the sim side the
    /// recorder is fed a modelled pass cost (`scenarios::overlong_pass`) and on
    /// the kernel side the real TSC.
    ///
    /// It lives on the handle rather than in the `CpuSched` because the
    /// measurement lands *after* the pass has consumed every borrow of that,
    /// and because a report has to be readable from outside a pass.
    #[cfg(feature = "check")]
    pass_costs: PassCosts,
}

impl<M: SchedMsg> CpuHandle<M> {
    pub fn new(id: CpuId, post: MailboxProducer<M>) -> Self {
        Self {
            id,
            post,
            doorbell: Doorbell::new(),
            load: AtomicU32::new(0),
            #[cfg(feature = "check")]
            pass_costs: PassCosts::new(),
        }
    }

    pub fn id(&self) -> CpuId {
        self.id
    }

    #[cfg(feature = "check")]
    pub fn pass_costs(&self) -> &PassCosts {
        &self.pass_costs
    }

    pub fn doorbell(&self) -> &Doorbell {
        &self.doorbell
    }

    pub fn load(&self) -> u32 {
        self.load.load(Ordering::Relaxed)
    }

    pub fn publish_load(&self, ready: u32) {
        self.load.store(ready, Ordering::Relaxed);
    }

    /// Post one message and ring the doorbell. The returned [`Kick`] is the
    /// caller's obligation: `Kick::Send` means the targeted IPI must go out
    /// (spec §7.3).
    pub fn post(
        &self,
        slot: PostSlot<'_, M>,
        msg: M,
        urgency: Urgency,
        preempt: &impl PreemptGuard,
    ) -> Kick {
        self.post.post(slot, msg, preempt);
        self.doorbell.ring(urgency)
    }

    /// Post a message that carries its own node — the ownership-transferring
    /// `Adopt` (spec §7.2).
    pub fn post_owned(
        &self,
        msg: M,
        node_of: fn(&M) -> &MailboxNode<M>,
        urgency: Urgency,
        preempt: &impl PreemptGuard,
    ) -> Kick {
        self.post.post_owned(msg, node_of, preempt);
        self.doorbell.ring(urgency)
    }
}

/// The boot-initialized slice of handles. Indexed by [`CpuId`]; an unknown
/// CPU id is a bug, not a lookup failure.
pub struct CpuHandles<M> {
    handles: Box<[CpuHandle<M>]>,
}

impl<M: SchedMsg> CpuHandles<M> {
    pub fn new(handles: Vec<CpuHandle<M>>) -> Self {
        for (index, handle) in handles.iter().enumerate() {
            assert_eq!(
                handle.id(),
                CpuId(index as u32),
                "cpu handles must be indexed by their own id",
            );
        }
        Self {
            handles: handles.into_boxed_slice(),
        }
    }

    pub fn get(&self, cpu: CpuId) -> &CpuHandle<M> {
        self.handles
            .get(cpu.0 as usize)
            .unwrap_or_else(|| panic!("no such cpu: {cpu:?}"))
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

#[cfg(all(test, feature = "check"))]
mod tests {
    use super::*;
    use alloc::format;

    /// The two halves of the wire form are one format, and this is what says
    /// so: a `Display` that gains a field and a `parse` that does not is a
    /// harness reading zeros out of a live machine and calling it green.
    #[test]
    fn a_report_survives_the_wire() {
        let mut report = PassCostReport::empty(CpuId(3));
        report.buckets[pass_cost_bucket(4_000)] = 900;
        report.buckets[pass_cost_bucket(1_684_167)] = 1;
        report.count = 901;
        report.max_ns = 1_684_167;
        report.over = 1;
        let line = format!("[kernel 1.234 cpu3] {report}");
        assert_eq!(PassCostReport::parse(&line), Some(report));
    }

    /// An empty histogram still round-trips, because a CPU that has taken no
    /// pass is a state the harness must be able to read rather than one it
    /// mistakes for a truncated line.
    #[test]
    fn an_empty_report_survives_the_wire() {
        let report = PassCostReport::empty(CpuId(0));
        assert_eq!(PassCostReport::parse(&format!("{report}")), Some(report));
    }

    /// A line whose histogram does not add up to its `n` is refused. The
    /// console splices lines under load, and a half-read report parsed as a
    /// whole one gates on a distribution that never existed.
    #[test]
    fn a_truncated_report_is_refused() {
        let mut report = PassCostReport::empty(CpuId(1));
        report.buckets[10] = 5;
        report.count = 900;
        assert_eq!(PassCostReport::parse(&format!("{report}")), None);
        assert_eq!(PassCostReport::parse("[kernel 1.0 cpu0] xhci: reset"), None);
        assert_eq!(
            PassCostReport::parse("sched-check pass-costs cpu=0 n=1 max=2 over=0"),
            None,
        );
    }

    /// Bucket `b` holds `[2^(b-1), 2^b)`, and the quantile reads back the
    /// bucket's *end*. Both directions of the boundary, because an off-by-one
    /// here is a gate that is one power of two too kind.
    #[test]
    fn a_bucket_is_a_power_of_two_wide() {
        assert_eq!(pass_cost_bucket(0), 0);
        assert_eq!(pass_cost_bucket(1), 1);
        assert_eq!(pass_cost_bucket(2), 2);
        assert_eq!(pass_cost_bucket(3), 2);
        assert_eq!(pass_cost_bucket(4), 3);
        assert_eq!(pass_cost_bucket(MAX_PASS_NS), 18);
        assert_eq!(pass_cost_bucket_end(18), 262_144);
        assert_eq!(pass_cost_bucket_end(17), 131_072);
        assert_eq!(pass_cost_bucket(u64::MAX), PASS_COST_BUCKETS - 1);
        assert_eq!(pass_cost_bucket_end(PASS_COST_BUCKETS - 1), u64::MAX);
    }

    /// The quantile is the whole of what the harness gates, so it is asked the
    /// question the harness asks: a bulk of cheap passes with one enormous
    /// sample must answer *cheap*, and a bulk of expensive ones must not.
    #[test]
    fn a_quantile_follows_the_mass_and_not_the_tail() {
        let mut sparse = PassCostReport::empty(CpuId(0));
        sparse.buckets[12] = 99_999; // < 4096 ns
        sparse.buckets[21] = 1; // ~2 ms, one host-stolen pass
        sparse.count = 100_000;
        sparse.max_ns = 1_900_000;
        sparse.over = 1;
        assert_eq!(sparse.quantile_upper_ns(99, 100), 4_096);
        assert_eq!(sparse.quantile_upper_ns(999, 1_000), 4_096);
        assert_eq!(sparse.quantile_upper_ns(1, 1), pass_cost_bucket_end(21));

        let mut heavy = PassCostReport::empty(CpuId(0));
        heavy.buckets[12] = 90_000;
        heavy.buckets[18] = 10_000; // a tenth of every pass over 131 µs
        heavy.count = 100_000;
        assert_eq!(heavy.quantile_upper_ns(99, 100), 262_144);
        assert_eq!(heavy.quantile_upper_ns(9, 10), 4_096);

        assert_eq!(PassCostReport::empty(CpuId(0)).quantile_upper_ns(1, 2), 0);
    }

    /// The recorder and the report agree, including the exact `over` count the
    /// histogram cannot express.
    #[test]
    fn the_recorder_counts_what_it_was_given() {
        let costs = PassCosts::new();
        for ns in [0, 1, 4_000, MAX_PASS_NS, MAX_PASS_NS + 1, 1_684_167] {
            costs.record(ns);
        }
        let report = costs.report(CpuId(2));
        assert_eq!(report.cpu, CpuId(2));
        assert_eq!(report.count, 6);
        assert_eq!(report.max_ns, 1_684_167);
        assert_eq!(report.over, 2);
        // Both budget samples land in bucket 18, `[131072, 262144)`, which is
        // exactly why `over` is counted separately: the histogram cannot tell
        // 200 000 from 200 001 and the assert this replaced could.
        assert_eq!(report.buckets[pass_cost_bucket(MAX_PASS_NS)], 2);
        assert_eq!(report.buckets.iter().sum::<u64>(), 6);
    }
}
