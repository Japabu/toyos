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
use alloc::collections::{BTreeMap, VecDeque};
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
    /// Killed tasks that still have a kernel stack to unwind, dispatched
    /// ahead of the **fair** queue and behind the RT band.
    ///
    /// **This is what replaces every reap-in-place** (spec §7.2 of
    /// `specs/completion-architecture-spec.md`): this kernel does not unwind,
    /// so a task whose value is discarded takes every guard on its stack with
    /// it — a sleep lock nobody can ever take again. A killed task is
    /// therefore *scheduled*, observes the cancel at its next park or at its
    /// return to userland, and dies by its own `die`.
    ///
    /// Separate from the fair queue rather than ordered inside it, for the
    /// bound: a dying task is not competing for a share of the CPU, it is
    /// releasing resources a retirer is blocked on, so its wait is one pick and
    /// not the depth of the fair band. That is what keeps invariant I14's
    /// retire bound a quantum-shaped number instead of a queue-shaped one.
    ///
    /// **The argument reaches the fair band and stops there.** "A retirer is
    /// blocked on what this task holds" says nothing about real-time work,
    /// which is not waiting on the corpse and whose own bound —
    /// `scheduler-core-spec.md` §3's "a ready real-time task always preempts
    /// the normal band", priced by invariant I4 — admits no exception. So
    /// [`SchedPass::pick`] serves `rq` first *whenever the RT band is
    /// occupied*, and a dying task is preempted for RT exactly as any other
    /// fair-band task is.
    ///
    /// **A queue and not a stack.** Two concurrent process teardowns put two
    /// corpses on one CPU; popping the end `keep_dying` pushes to would
    /// re-select the newest on every pick and the older one would never run —
    /// so the bound above would be false for k > 1, which is the only k that
    /// makes it a bound at all.
    dying: VecDeque<ReadyTask<X>>,
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
            dying: VecDeque::new(),
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

    /// The killed tasks waiting to unwind, for the invariant walks and for a
    /// dump that has to say where every task is.
    pub fn dying(&self) -> impl Iterator<Item = &ReadyTask<X>> + '_ {
        self.dying.iter()
    }

    pub fn dying_len(&self) -> usize {
        self.dying.len()
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
            // **A killed task is still never migrated**, and that half of
            // invariant I14 is unchanged by §7.2: `InTransit` is the one state
            // whose handling is not backed by an interrupt, so handing a
            // corpse on trades an unwind that could start in this pass for a
            // wait on another CPU's next voluntary one. What changes is only
            // what happens to it here — it is kept and dispatched, not reaped.
            self.begin_dying(task, env);
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
        // **The RT forward is decided first, and the kill check stays inside
        // `hand_off`.** Both orders keep a killed task off another CPU, but
        // only this one leaves `hand_off`'s check on the path a wake-forward
        // takes — which is the path `old_migrate_kept_the_corpse` stages, and
        // a negative gate that has become unreachable is a gate that has been
        // weakened.
        if task.is_rt() && self.running.as_ref().is_some_and(|r| r.is_rt()) {
            if let Some(dst) = self.idle_sibling(env) {
                self.hand_off(task, dst, env, now);
                return;
            }
        }
        if task.shared().kill_pending() {
            // A dying task is not queued behind work: it is dispatched next,
            // so its unwind starts inside this pass's own pick.
            self.begin_dying(task, env);
            return;
        }
        self.enqueue(task, env);
    }

    /// Put a killed task where the pick takes it first, counting it into its
    /// share exactly as [`Self::enqueue`] would.
    ///
    /// The refcount is the reason this is not a bare `push`: `Ready` and
    /// `Running` both count as runnable, so a dying task that skipped
    /// `enter_runnable` would desynchronise the per-share count the sim walks
    /// in `check_share_refcounts` — which is §7.2(a)'s warning about the
    /// struck replacement code, one container over.
    fn begin_dying<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        task: ReadyTask<X>,
        env: Env<'_, H, P>,
    ) {
        let _vruntime = task.share().enter_runnable(env.frontier);
        self.keep_dying(task);
    }

    /// The same, for a task that is *already* counted — one taken out of the
    /// run queue, which never left it.
    ///
    /// Both routes end a borrowed RT window: [`ReadyTask::end_lend`] carries
    /// the argument, and this is the one place that has to remember it.
    fn keep_dying(&mut self, mut task: ReadyTask<X>) {
        task.end_lend();
        self.dying.push_back(task);
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
        let ready = task.adopt(self.id, now);
        self.trace(env, now, TraceKind::Adopt { task: key });
        // Killed while in flight lands in the dying list rather than the run
        // queue, through `place`. The retire chase still terminates and for a
        // sharper reason: whoever ends up owning the task *dispatches* it, and
        // it dies by its own hand at the first safe point that can end it.
        self.place(ready, env, now);
    }

    fn handle_retire<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        shared: &Arc<TaskShared<Msg<X>>>,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        let key = shared.key();
        if self.parked.contains_key(&key) {
            // **Claim-arbitrated, exactly as `fire_deadlines` is** (§7.2(c)):
            // remove-then-convert loses the race. If a remote waker has
            // already claimed this task its `Msg::Wake` is in flight to this
            // same CPU, so leaving the entry alone is what keeps the task in
            // *some* container — `handle_wake` finds it and the wake places
            // it, into the dying list, because the kill bit is already set.
            let entry = self.parked.get_mut(&key).expect("just checked");
            match entry.task.shared().claim_wake() {
                Claim::Parked(cpu) => {
                    assert_eq!(cpu, self.id, "a task parked here claims another CPU");
                    let entry = self.parked.remove(&key).expect("still there");
                    let task = entry.task.wake(
                        self.id,
                        WakeCause::new(WakeReason::Woken),
                        entry.class,
                        now,
                    );
                    self.trace(env, now, TraceKind::Wake { task: key });
                    self.place(task, env, now);
                }
                Claim::PrePark => panic!("a parked task cannot be pre-park"),
                Claim::Lost => {}
            }
            return;
        }
        if let Some(ready) = self.rq.remove(key) {
            // Out of the fair queue and into the dying list. No refcount
            // movement: it was runnable in the queue and it is runnable here.
            self.trace(env, now, TraceKind::Wake { task: key });
            self.keep_dying(ready);
            return;
        }
        if let Some(current) = self.running.as_mut().filter(|r| r.key() == key) {
            // The borrowed window ends here, for the reason
            // `ReadyTask::end_lend` gives — and this is the arm where a victim
            // never reaches the dying list at all: nothing takes the CPU away
            // from a running task whose quantum has not expired, so it unwinds
            // in place, and a lend left armed would spend the producer's
            // priority on a corpse.
            current.end_lend();
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

/// How long one scheduler pass may take on the machine it runs on (spec
/// §10.2, §14's preempt-off risk). Asserted only in `feature = "check"`
/// builds, which is what the kernel's `sched-check` feature turns on.
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
/// design forces 200 µs. If it ever fires on honest work, the honest response
/// is to find out which pass grew and why — not to raise it.
pub const MAX_PASS_NS: u64 = 200_000;

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
            if task.shared().kill_pending() {
                self.cpu.keep_dying(task);
                return self.dispose();
            }
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
        // only in a check build and feeds an assert rather than a decision.
        #[cfg(feature = "check")]
        let (hw, entered) = (self.env.hw, self.now);
        let action = self.finish_inner();
        #[cfg(feature = "check")]
        check_pass_duration(hw, entered);
        action
    }

    fn finish_inner(mut self) -> Action<<H as Hw>::Payload> {
        loop {
            self.preempt_if_due();
            self.pick();
            self.answer_steal_requests();
            // **The dying list counts.** The published load is what spawn
            // placement and the steal probe read as "how much work is on this
            // CPU", and a corpse mid-unwind is work: it is dispatched ahead of
            // the fair band, so a task placed here queues behind it. Counting
            // `rq` alone made a CPU holding two teardowns look as empty as an
            // idle one — the same blindness `dying_len` closes in the dump.
            self.env
                .cpus
                .get(self.cpu.id)
                .publish_load((self.cpu.rq.len() + self.cpu.dying.len()) as u32);
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
        if task.shared().kill_pending() {
            // §7.2(3): once `Commit::Killed` is `dispose_none` the killed
            // thread keeps running and unwinds, and its next quantum expiry
            // must not put it where something else can take it — the pick
            // reaped it here in the struck design, mid-unwind, with every
            // guard still on the stack. It goes to the back of the dying list,
            // which the next pick empties ahead of the fair band.
            //
            // **Ahead of the fair band and not of the RT one**, which is the
            // half that makes this arm honour rather than undo the decision
            // that reached it: when what fired this preemption is `rq.has_rt()`
            // the pick serves that RT task, and this corpse waits with the fair
            // band it belongs to. Deleting this arm is caught by
            // `a_killed_task_that_expires_its_quantum_goes_back_to_the_dying_list`
            // — the fair queue is where the task would land instead, and that
            // test asserts it never does.
            self.cpu.keep_dying(task);
            return;
        }
        let vruntime = task
            .share()
            .runnable_vruntime()
            .expect("a preempted task's share must be runnable");
        self.cpu.rq.insert(vruntime, task);
    }

    /// **RT band, then the dying list, then the fair band — and no kill check
    /// at all on the fair path.**
    ///
    /// §7.2(2): the pick used to reap a killed ready task, which is what made
    /// the earlier drafts' `handle_retire` rewrite a no-op — a task pushed
    /// into `rq` by the retire was popped and reaped in the very same pass,
    /// stack and guards discarded, the disaster moved fifteen lines later.
    /// Now a killed task is dispatched like any other, and it dies by its own
    /// `die` at the first safe point that can end it. Nothing is reaped here,
    /// so `ReadyTask::dispatch`'s note about the kill bit not being asserted
    /// away is the whole of what remains true.
    ///
    /// A dying task jumps the *fair* queue and the vruntime frontier does not
    /// advance for it: it is not spending a share of the CPU, it is finishing.
    ///
    /// **It does not jump the RT band, and taking `dying` unconditionally is
    /// what made it.** `rq.pop_next()` is the only place the RT band is served,
    /// so a pick that emptied `dying` first left a killed normal task holding
    /// the CPU against a ready real-time task for the whole of its unwind —
    /// quantum after quantum, because `preempt_if_due` returns it to `dying`
    /// and this pick handed it straight back with a fresh quantum. That
    /// contradicts `scheduler-core-spec.md` §3 outright, and the retirer's
    /// claim on the corpse's resources is no argument for it: the RT task is
    /// not the retirer and is not waiting on anything the corpse holds. So the
    /// question asked here is `rq.has_rt()`, and the dying list is served only
    /// when the answer is no. `a_killed_task_does_not_starve_a_ready_rt_task`
    /// and its two siblings are the gates.
    fn pick(&mut self) {
        if self.cpu.running.is_some() {
            return;
        }
        if !self.cpu.rq.has_rt() {
            if let Some(task) = self.cpu.dying.pop_front() {
                let key = task.key();
                self.cpu.running = Some(task.dispatch(self.cpu.id, self.now));
                self.cpu.quantum_end = self.now.after(QUANTUM_NS);
                self.cpu
                    .trace(self.env, self.now, TraceKind::Schedule { task: key });
                return;
            }
        }
        // A lapsed borrowed window must not demote the task out of the RT band
        // here: queue time spends none of it, so `ReadyTask::dispatch` re-arms
        // it instead. The band stays whatever it was at insert.
        if let Some((vruntime, task)) = self.cpu.rq.pop_next() {
            self.env.frontier.advance(vruntime);
            let key = task.key();
            self.cpu.running = Some(task.dispatch(self.cpu.id, self.now));
            self.cpu.quantum_end = self.now.after(QUANTUM_NS);
            self.cpu
                .trace(self.env, self.now, TraceKind::Schedule { task: key });
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

/// The on-target counterpart to the simulator's invariants (spec §10.2): the
/// sim asserts what a pass *does*, this asserts what a pass *costs*.
///
/// Everything else in `feature = "check"` is a statement about state the core
/// owns and can therefore be checked in either world. This one cannot: the
/// simulator's clock does not advance inside a step, so on the sim side it is
/// exercised by a modelled pass cost (`scenarios::overlong_pass`) and on the
/// kernel side by the real TSC.
#[cfg(feature = "check")]
fn check_pass_duration<H: Hw>(hw: &H, entered: Nanos) {
    let elapsed = hw.now().since(entered);
    assert!(
        elapsed <= MAX_PASS_NS,
        "invariant P: a scheduler pass took {elapsed} ns, budget {MAX_PASS_NS} ns — \
         the simulator charges a pass nothing, so invariant I4's RT wake-latency \
         bound is optimistic by at least that much",
    );
}

/// The globally shared, `Sync` face of a CPU, and the whole remote surface:
/// post a message, ring the doorbell, read the published load (spec §6.1).
pub struct CpuHandle<M> {
    id: CpuId,
    post: MailboxProducer<M>,
    doorbell: Doorbell,
    /// Ready-count heuristic, published for spawn placement only.
    load: AtomicU32,
}

impl<M: SchedMsg> CpuHandle<M> {
    pub fn new(id: CpuId, post: MailboxProducer<M>) -> Self {
        Self {
            id,
            post,
            doorbell: Doorbell::new(),
            load: AtomicU32::new(0),
        }
    }

    pub fn id(&self) -> CpuId {
        self.id
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

/// The arms this file has that nothing else covers.
///
/// **`cpu.rs` had no test module at all before
/// `specs/completion-architecture-spec.md` §7.3 asked for one**, and every arm
/// exercised below — the retire's three, the pick's, the balance path's and
/// the adopt's — was reachable only through the simulator, which explores
/// *scenarios* rather than stating what a single arm does. The scheduler
/// migration cost seventy defects in code whose own suites were green
/// (`specs/assessments/metal-track-history.md`); these are the statements a
/// reader can check one at a time.
///
/// The harness is deliberately the smallest thing that can hold a `CpuSched`:
/// a payload with no address space, an `Hw` that records rather than acts, and
/// one CPU unless a test needs two.
#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::fair::{FairShare, ShareState};
    use crate::hw::{Kicker, Machine};
    use crate::mailbox::{mailbox, NoPreempt};
    use crate::sync::LeafLock;
    use crate::task::{RtState, TaskAccounting, TaskBuilder};
    use crate::waitq::{WaitList, WaitQueue};
    use std::sync::Mutex;

    struct TestLock<T>(Mutex<T>);

    impl<T: Send> LeafLock<T> for TestLock<T> {
        fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
            f(&mut self.0.lock().expect("a test never poisons a lock"))
        }
    }

    struct TestPayload;

    impl SchedPayload for TestPayload {
        type Ctx = ();
        type ShareLock = TestLock<ShareState>;
    }

    #[derive(Default)]
    struct HwState {
        released: Vec<TaskKey>,
        need_resched: Vec<CpuId>,
        kicks: Vec<CpuId>,
        switches: Vec<Option<TaskKey>>,
    }

    #[derive(Default)]
    struct TestHw(Mutex<HwState>);

    impl TestHw {
        fn state(&self) -> std::sync::MutexGuard<'_, HwState> {
            self.0.lock().expect("a test never poisons a lock")
        }
    }

    impl Kicker for TestHw {
        fn kick(&self, target: CpuId) {
            self.state().kicks.push(target);
        }
    }

    impl Machine for TestHw {
        type IrqGuard = ();
        fn now(&self) -> Nanos {
            Nanos::ZERO
        }
        fn set_timer(&self, _deadline: Nanos) {}
        fn stop_timer(&self) {}
        fn irq_guard(&self) {}
        fn halt(&self) {}
        fn need_resched(&self, cpu: CpuId) {
            self.state().need_resched.push(cpu);
        }
        fn trace(&self, _ev: TraceEvent) {}
    }

    impl Hw for TestHw {
        type Payload = TestPayload;
        #[allow(unsafe_code)] // the declaration is unsafe; this body reads keys only
        unsafe fn switch(&self, token: RunToken<TestPayload>) {
            self.state().switches.push(token.incoming());
        }
        fn release(&self, key: TaskKey, _payload: TestPayload, _acct: TaskAccounting) {
            self.state().released.push(key);
        }
    }

    const C0: CpuId = CpuId(0);
    const C1: CpuId = CpuId(1);
    const NOW: Nanos = Nanos(1_000);

    /// One CPU's worth of world, plus the handles both CPUs need.
    struct World {
        cpus: Vec<CpuSched<TestPayload>>,
        handles: CpuHandles<Msg<TestPayload>>,
        hw: TestHw,
        frontier: Frontier,
        preempt: NoPreempt,
        next_key: u64,
    }

    impl World {
        fn new(count: usize) -> Self {
            let mut cpus = Vec::new();
            let mut handles = Vec::new();
            for i in 0..count {
                let (tx, rx) = mailbox();
                cpus.push(CpuSched::new(CpuId(i as u32), rx, ()));
                handles.push(CpuHandle::new(CpuId(i as u32), tx));
            }
            Self {
                cpus,
                handles: CpuHandles::new(handles),
                hw: TestHw::default(),
                frontier: Frontier::new(),
                preempt: NoPreempt,
                next_key: 1,
            }
        }

        /// The CPUs and the environment as two disjoint borrows. One call
        /// rather than two accessors because `Env` borrows every field the
        /// CPUs do not, and the compiler only sees that inside one body.
        fn split(
            &mut self,
        ) -> (
            &mut Vec<CpuSched<TestPayload>>,
            Env<'_, TestHw, NoPreempt>,
        ) {
            (
                &mut self.cpus,
                Env {
                    hw: &self.hw,
                    cpus: &self.handles,
                    frontier: &self.frontier,
                    preempt: &self.preempt,
                    steal: false,
                },
            )
        }

        /// A task in transit to `dst`, which is where every task starts.
        fn spawn(&mut self, dst: CpuId) -> (TaskKey, Arc<TaskShared<Msg<TestPayload>>>) {
            self.spawn_with(dst, RtState::default())
        }

        /// The same, permanently in the RT band — the band the dying list must
        /// not outrank.
        fn spawn_rt(&mut self, dst: CpuId) -> (TaskKey, Arc<TaskShared<Msg<TestPayload>>>) {
            self.spawn_with(
                dst,
                RtState {
                    permanent: true,
                    inherited: None,
                    lends: 0,
                },
            )
        }

        fn spawn_with(
            &mut self,
            dst: CpuId,
            rt: RtState,
        ) -> (TaskKey, Arc<TaskShared<Msg<TestPayload>>>) {
            let key = TaskKey(self.next_key);
            self.next_key += 1;
            let share = Arc::new(FairShare::new(TestLock(Mutex::new(
                ShareState::NonRunnable { lag: 0 },
            ))));
            let task = TaskBuilder {
                key,
                share,
                ctx: (),
                ext: TestPayload,
                rt,
            }
            .build(dst, NOW);
            let shared = task.shared().clone();
            let (cpus, env) = self.split();
            cpus[dst.0 as usize].handle_adopt(task, env, NOW);
            (key, shared)
        }

        /// Dispatch whatever the pick chooses, so a test can put a task in
        /// `running` without reaching into the CPU.
        fn run_a_pass(&mut self, cpu: CpuId) {
            self.run_a_pass_at(cpu, NOW);
        }

        /// The same at a chosen instant, for the tests that have to let a
        /// quantum expire.
        fn run_a_pass_at(&mut self, cpu: CpuId, now: Nanos) {
            let (cpus, env) = self.split();
            let pass = SchedPass::begin(&mut cpus[cpu.0 as usize], env, now);
            let _ = pass.dispose_none().finish();
        }

        fn park_running(&mut self, cpu: CpuId, queue: &WaitQueue<Msg<TestPayload>, TestLock<WaitList<Msg<TestPayload>>>>) {
            let (cpus, env) = self.split();
            let sched = &mut cpus[cpu.0 as usize];
            let current = CurrentTask::new(
                sched.running().expect("a running task to park").shared(),
                cpu,
            );
            let ticket = queue.prepare_wait(&current);
            let (committed, registration) = match ticket.commit() {
                crate::waitq::Commit::Parked(c, r) => (c, r),
                _ => panic!("the commit refused an uncontended park"),
            };
            let pass = SchedPass::begin(sched, env, NOW);
            let _ = pass.dispose_block(committed, None).finish();
            core::mem::forget(registration);
        }

        fn released(&self) -> Vec<TaskKey> {
            self.hw.state().released.clone()
        }

        /// End a test that deliberately leaves a task alive.
        ///
        /// `Task`'s drop bomb — "the only legal death is `DeadTask::finalize`"
        /// — is a scheduler invariant and not a cleanup path, so a world with
        /// a live task in it may not be dropped. Forgetting it is what a
        /// running machine does with a task that is still running.
        fn abandon(self) {
            core::mem::forget(self);
        }
    }

    fn queue() -> WaitQueue<Msg<TestPayload>, TestLock<WaitList<Msg<TestPayload>>>> {
        WaitQueue::new(WaitClass::Other, TestLock(Mutex::new(WaitList::new())))
    }

    /// A task reaches `running` through the ordinary route: adopt, pass, pick.
    #[test]
    fn a_spawned_task_is_adopted_and_dispatched() {
        let mut w = World::new(1);
        let (key, shared) = w.spawn(C0);
        assert_eq!(shared.state(), TaskState::Ready(C0), "adopt makes it ready");
        w.run_a_pass(C0);
        assert_eq!(shared.state(), TaskState::Running(C0));
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(key));
        w.abandon();
    }

    /// **The arm §7.1 calls the one that matters, rewritten.** A thread parked
    /// on a disk transfer is in `parked`; it used to be reaped where it lay,
    /// and its kernel stack — every guard on it — went with it. Now the retire
    /// *wakes* it, claim-arbitrated, into the dying list.
    #[test]
    fn a_retire_wakes_a_parked_task_so_it_can_unwind() {
        let mut w = World::new(1);
        let q = queue();
        let (key, shared) = w.spawn(C0);
        w.run_a_pass(C0);
        w.park_running(C0, &q);
        assert_eq!(shared.state(), TaskState::Blocked(C0));

        crate::retire::begin(&shared).post(&w.handles, &w.hw, &NoPreempt);
        {
            let (cpus, env) = w.split();
            cpus[0].drain(env, NOW);
        }

        assert!(w.released().is_empty(), "nothing was discarded");
        assert_eq!(w.cpus[0].dying.len(), 1, "it is waiting to unwind");
        assert_eq!(w.cpus[0].dying[0].key(), key);
        assert_eq!(shared.state(), TaskState::Ready(C0));
        w.abandon();
    }

    /// The claim is arbitrated and not assumed: a remote waker that got there
    /// first owns a `Msg::Wake` in flight to this same CPU, so the retire
    /// leaves the entry alone. Removing it here would leave the task in no
    /// container at all — never runnable, never reaped — which is §7.2(c).
    #[test]
    fn a_retire_that_loses_the_claim_leaves_the_wake_in_flight() {
        let mut w = World::new(1);
        let q = queue();
        let (key, shared) = w.spawn(C0);
        w.run_a_pass(C0);
        w.park_running(C0, &q);

        // A waker wins the claim first; its message is queued to cpu0.
        assert!(crate::waitq::wake_direct(
            &shared,
            WakeCause::new(WakeReason::Woken),
            &w.handles,
            &w.hw,
            &NoPreempt,
        ));
        crate::retire::begin(&shared).post(&w.handles, &w.hw, &NoPreempt);
        {
            let (cpus, env) = w.split();
            cpus[0].drain(env, NOW);
        }

        assert!(w.released().is_empty());
        assert_eq!(w.cpus[0].dying.len(), 1, "the in-flight wake placed it");
        assert_eq!(w.cpus[0].dying[0].key(), key);
        w.abandon();
    }

    /// The same hazard one step later: woken by a release, sitting in the run
    /// queue with the previous guard still on its stack, killed before it is
    /// picked. Out of the fair queue, into the dying list, no refcount
    /// movement — it was runnable there and it is runnable here.
    #[test]
    fn a_retire_moves_a_ready_task_to_the_dying_list() {
        let mut w = World::new(1);
        let (key, shared) = w.spawn(C0);
        assert_eq!(shared.state(), TaskState::Ready(C0));

        crate::retire::begin(&shared).post(&w.handles, &w.hw, &NoPreempt);
        {
            let (cpus, env) = w.split();
            cpus[0].drain(env, NOW);
        }

        assert!(w.released().is_empty());
        assert!(w.cpus[0].rq.is_empty(), "it is not in the fair queue any more");
        assert_eq!(w.cpus[0].dying.len(), 1);
        assert_eq!(w.cpus[0].dying[0].key(), key);
        w.abandon();
    }

    /// The one arm that always did what §7.2 wants of all of them: a running
    /// task cannot be yanked out from under its own kernel stack, so it is
    /// asked to take a safe point instead.
    #[test]
    fn a_retire_of_the_running_task_asks_for_a_safe_point() {
        let mut w = World::new(1);
        let (_key, shared) = w.spawn(C0);
        w.run_a_pass(C0);
        assert_eq!(shared.state(), TaskState::Running(C0));

        crate::retire::begin(&shared).post(&w.handles, &w.hw, &NoPreempt);
        w.run_a_pass(C0);

        assert_eq!(shared.state(), TaskState::Running(C0), "it keeps its stack");
        assert!(w.released().is_empty(), "nothing was released");
        assert_eq!(w.hw.state().need_resched, std::vec![C0]);
        w.abandon();
    }

    /// **The pick no longer reaps anything.** A killed task is dispatched like
    /// any other and dies by its own `die`; reaping it here is what made the
    /// earlier drafts of §7.2 a no-op, since a task the retire had just made
    /// runnable was popped and discarded in the very same pass.
    #[test]
    fn the_pick_dispatches_a_killed_task_so_it_can_unwind() {
        let mut w = World::new(1);
        let (key, shared) = w.spawn(C0);
        shared.mark_kill();

        w.run_a_pass(C0);

        assert!(w.released().is_empty(), "nothing was reaped");
        assert_eq!(shared.state(), TaskState::Running(C0));
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(key));
        w.abandon();
    }

    /// A dying task is picked before the fair queue: it is not competing for
    /// the CPU, it is releasing resources a retirer is blocked on, and that is
    /// what keeps the retire bound a quantum-shaped number.
    #[test]
    fn a_dying_task_is_picked_before_the_fair_queue() {
        let mut w = World::new(1);
        let (_first, _shared_a) = w.spawn(C0);
        let (dying_key, dying_shared) = w.spawn(C0);
        dying_shared.mark_kill();
        {
            let (cpus, env) = w.split();
            let task = cpus[0].rq.remove(dying_key).expect("ready");
            let _ = env;
            cpus[0].keep_dying(task);
        }

        w.run_a_pass(C0);

        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(dying_key),
            "the dying task jumps the queue",
        );
        w.abandon();
    }

    /// The unwind ends in the ordinary exit, and *that* is what releases the
    /// payload — one death for every task, on the CPU it was running on.
    #[test]
    fn a_dying_task_that_exits_is_released_by_its_own_death() {
        let mut w = World::new(1);
        let (key, shared) = w.spawn(C0);
        shared.mark_kill();
        w.run_a_pass(C0);
        assert_eq!(shared.state(), TaskState::Running(C0));

        {
            let (cpus, env) = w.split();
            let pass = SchedPass::begin(&mut cpus[0], env, NOW);
            let _ = pass.dispose_exit().finish();
        }
        // The zombie is freed by the next pass, which is not standing on its
        // stack.
        w.run_a_pass(C0);

        assert_eq!(shared.state(), TaskState::Dead);
        assert_eq!(w.released(), std::vec![key]);
    }

    /// §7.2(3): a killed task that expires its quantum mid-unwind must not
    /// land anywhere the pick can treat it as ordinary work.
    #[test]
    fn a_killed_task_that_expires_its_quantum_goes_back_to_the_dying_list() {
        let mut w = World::new(1);
        let (key, shared) = w.spawn(C0);
        w.run_a_pass(C0);
        shared.mark_kill();

        {
            let (cpus, env) = w.split();
            let pass = SchedPass::begin(&mut cpus[0], env, Nanos(NOW.0 + QUANTUM_NS + 1));
            let _ = pass.dispose_none().finish();
        }

        assert!(w.released().is_empty());
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(key),
            "picked straight back off the dying list",
        );
        assert!(w.cpus[0].rq.is_empty(), "and never through the fair queue");
        w.abandon();
    }

    /// **I14's first half is unchanged by §7.2**: a killed task is still never
    /// migrated, because `InTransit` is the one state whose handling is not
    /// backed by an interrupt. What changed is only what happens to it here —
    /// kept and dispatched, where it used to be reaped.
    #[test]
    fn the_balance_path_keeps_a_killed_task_rather_than_migrating_it() {
        let mut w = World::new(2);
        let (key, shared) = w.spawn(C0);
        shared.mark_kill();

        {
            let (cpus, env) = w.split();
            let task = cpus[0].rq.remove(key).expect("ready on cpu0");
            task.share().leave_runnable(env.frontier);
            cpus[0].hand_off(task, C1, env, NOW);
        }

        assert!(w.released().is_empty());
        assert_eq!(shared.state(), TaskState::Ready(C0), "still here");
        assert_eq!(w.cpus[0].dying.len(), 1);
        w.abandon();
    }

    /// A kill that lands after the adopt was posted: the destination adopts it
    /// like any other task and dispatches it, which is what makes the retire
    /// chase terminate — for a sharper reason than the reap it replaces.
    #[test]
    fn an_adopt_of_a_killed_task_dispatches_it_on_arrival() {
        let mut w = World::new(2);
        let (key, shared) = w.spawn(C0);

        let (cpus, env) = w.split();
        let task = cpus[0].rq.remove(key).expect("ready on cpu0");
        task.share().leave_runnable(env.frontier);
        cpus[0].hand_off(task, C1, env, NOW);
        assert_eq!(shared.state(), TaskState::InTransit(C1));

        shared.mark_kill();
        w.run_a_pass(C1);

        assert!(w.released().is_empty(), "nothing was discarded in flight");
        assert_eq!(
            w.cpus[1].running().map(|t| t.key()),
            Some(key),
            "adopted, placed in the dying list, and picked in the same pass",
        );
        w.abandon();
    }

    /// The control the three RT tests below are read against: an *ordinary*
    /// fair task loses the CPU to a ready RT task at the next pass.
    #[test]
    fn a_live_fair_task_loses_the_cpu_to_a_ready_rt_task() {
        let mut w = World::new(1);
        let (fair, _fair_shared) = w.spawn(C0);
        w.run_a_pass(C0);
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(fair));

        let (rt, _rt_shared) = w.spawn_rt(C0);
        w.run_a_pass_at(C0, Nanos(NOW.0 + 1));

        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(rt));
        w.abandon();
    }

    /// **A dying task is fair-band work, not a band of its own.** It jumps the
    /// fair queue because a retirer is blocked on what it holds; it does not
    /// jump the RT band, because nothing about an unwind makes it more urgent
    /// than real-time work — and spec §3's "a ready real-time task always
    /// preempts the normal band" admits no exception for it.
    ///
    /// The pass that fires because the RT task is ready must not hand the CPU
    /// straight back to the corpse: `preempt_if_due` takes it off, and the pick
    /// then serves `rq` — which is where the RT band lives — before `dying`.
    #[test]
    fn a_killed_task_does_not_starve_a_ready_rt_task() {
        let mut w = World::new(1);
        let (killed, killed_shared) = w.spawn(C0);
        w.run_a_pass(C0);
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(killed));
        killed_shared.mark_kill();

        let (rt, _rt_shared) = w.spawn_rt(C0);
        assert!(w.cpus[0].rq.has_rt(), "the RT task is ready on cpu0");

        w.run_a_pass_at(C0, Nanos(NOW.0 + 1));

        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(rt),
            "the RT task got the CPU on the first pass after it became ready",
        );
        assert_eq!(w.cpus[0].dying_len(), 1, "the corpse is queued, not running");
        assert!(w.released().is_empty(), "and nothing was discarded");
        w.abandon();
    }

    /// The same inversion driven by quantum expiry rather than by the RT
    /// preemption arm — the other of the two reasons `preempt_if_due` fires.
    #[test]
    fn a_killed_task_that_expires_its_quantum_yields_to_a_ready_rt_task() {
        let mut w = World::new(1);
        let (_killed, killed_shared) = w.spawn(C0);
        w.run_a_pass(C0);
        killed_shared.mark_kill();
        let (rt, _rt_shared) = w.spawn_rt(C0);

        w.run_a_pass_at(C0, Nanos(NOW.0 + QUANTUM_NS + 1));

        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(rt),
            "the expiring quantum is not a fresh one for the corpse",
        );
        assert_eq!(w.cpus[0].dying_len(), 1);
        w.abandon();
    }

    /// And the unwind is deferred, never dropped: once the RT band empties the
    /// dying task is picked again, still ahead of the fair queue.
    #[test]
    fn a_dying_task_resumes_when_the_rt_band_empties() {
        let mut w = World::new(1);
        let (killed, killed_shared) = w.spawn(C0);
        let (_fair, _fair_shared) = w.spawn(C0);
        w.run_a_pass(C0);
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(killed));
        killed_shared.mark_kill();
        let (rt, _rt_shared) = w.spawn_rt(C0);
        w.run_a_pass_at(C0, Nanos(NOW.0 + 1));
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(rt));

        // The RT task ends. Its own pass's pick is what takes the dying task.
        {
            let (cpus, env) = w.split();
            let pass = SchedPass::begin(&mut cpus[0], env, Nanos(NOW.0 + 2));
            let _ = pass.dispose_exit().finish();
        }

        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(killed),
            "the unwind resumes, and still ahead of the fair queue",
        );
        assert_eq!(w.cpus[0].rq.fair_len(), 1, "the fair task is still waiting");
        w.abandon();
    }

    /// **The dying list is a queue and not a stack.** Two concurrent process
    /// teardowns put two corpses on one CPU; a LIFO would re-select the newest
    /// on every pick and the older one would never run, which is exactly the
    /// bound the field's own doc denies.
    #[test]
    fn the_dying_list_is_served_oldest_first() {
        let mut w = World::new(1);
        let (first, first_shared) = w.spawn(C0);
        let (second, second_shared) = w.spawn(C0);
        first_shared.mark_kill();
        second_shared.mark_kill();
        {
            let (cpus, _env) = w.split();
            let task = cpus[0].rq.remove(first).expect("ready");
            cpus[0].keep_dying(task);
            let task = cpus[0].rq.remove(second).expect("ready");
            cpus[0].keep_dying(task);
        }

        w.run_a_pass(C0);
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(first),
            "the one that has been waiting longest unwinds first",
        );

        {
            let (cpus, env) = w.split();
            let pass = SchedPass::begin(&mut cpus[0], env, Nanos(NOW.0 + 1));
            let _ = pass.dispose_exit().finish();
        }
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(second),
            "and the other one follows it, rather than waiting on it forever",
        );
        w.abandon();
    }
}
