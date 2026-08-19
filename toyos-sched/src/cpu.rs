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

/// One killed task waiting to unwind, and when it started waiting.
///
/// The stamp is what makes the real-time band's precedence over it *bounded*
/// rather than absolute — see [`DYING_AGE_NS`]. It is refreshed every time the
/// corpse re-enters the list, so a corpse that has just spent an aged chunk goes
/// to the back of the queue and to the back of the clock at once, and the RT
/// band's next interruption is a full `DYING_AGE_NS` away.
struct Corpse<X: SchedPayload> {
    since: Nanos,
    task: ReadyTask<X>,
}

/// One CPU's complete scheduler state. `!Sync` and `!Send`: it is reachable
/// only through the CPU that owns it.
pub struct CpuSched<X: SchedPayload> {
    id: CpuId,
    running: Option<RunningTask<X>>,
    rq: RunQueue<X>,
    parked: BTreeMap<TaskKey, ParkedEntry<X>>,
    /// Killed tasks that still have a kernel stack to unwind, dispatched
    /// ahead of the **fair** queue, and behind the RT band only until the head
    /// of this list has aged ([`DYING_AGE_NS`]).
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
    /// not the depth of the fair band.
    ///
    /// **It jumps that queue and it does not escape its own.** Invariant I14's
    /// bound is queue-shaped in *this* container, and its own derivation says
    /// so: the term is `(1 + peers)`, where `peers` is the depth of this list
    /// on this CPU, and it is workload-shaped exactly as invariant I5's factor
    /// is. The sentence that used to end here said the opposite — "a
    /// quantum-shaped number instead of a queue-shaped one" — and it was
    /// written before that term existed.
    ///
    /// **The argument reaches the fair band and stops there, and what happens
    /// past it is a bounded deferral rather than an absolute.** "A retirer is
    /// blocked on what this task holds" says nothing about real-time work,
    /// which is not waiting on the corpse. So [`SchedPass::pick`] serves `rq`
    /// first *unless the head of this list has aged* ([`DYING_AGE_NS`]), and
    /// [`SchedPass::preempt_if_due`]'s RT arm does not fire against an aged
    /// corpse inside its [`DYING_CHUNK_NS`] grant — the two halves of one rule,
    /// which is why neither may be read as "exactly like any other fair-band
    /// task". The sentence that used to end here said exactly that, and said
    /// the pick serves `rq` first whenever the RT band is occupied; both were
    /// written before the grant existed and both were false the moment it did.
    ///
    /// **Superseded in design by `specs/scheduling-reservations-spec.md`**,
    /// which deletes the age, the chunk and the grant together: the deferral
    /// above is bounded per *corpse* and not per CPU, so k corpses take k
    /// consecutive grants, and a real-time band that briefly empties throws the
    /// accumulated age away on every dispatch. What replaces it is a per-CPU
    /// dying server holding an ordinary reservation, whose guarantee does not
    /// depend on how many corpses are behind it or on how the other band's
    /// occupancy happens to fall.
    ///
    /// **A queue and not a stack.** Two concurrent process teardowns put two
    /// corpses on one CPU; popping the end `keep_dying` pushes to would
    /// re-select the newest on every pick and the older one would never run —
    /// so the bound above would be false for k > 1, which is the only k that
    /// makes it a bound at all.
    dying: VecDeque<Corpse<X>>,
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
    /// The running task is a corpse that was dispatched **ahead of a ready
    /// real-time task** because it had aged, and holds the CPU until
    /// [`Self::quantum_end`] — one [`DYING_CHUNK_NS`] — against that band.
    ///
    /// Without this the grant is worth nothing: `preempt_if_due`'s RT arm fires
    /// at the *next pass*, not at the chunk boundary, so any interrupt landing
    /// inside the chunk would hand the CPU straight back and the corpse would
    /// make no progress at all under a device-interrupt storm. The grant is
    /// bounded by the quantum arm, which is what keeps invariant I4's new term
    /// exactly one chunk wide.
    aged_grant: bool,
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
    /// Negative-gate escape hatch only; see
    /// [`CpuSched::set_rt_outranks_every_corpse`].
    #[cfg(feature = "protocol-port")]
    rt_outranks_every_corpse: bool,
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
            aged_grant: false,
            loaded: Loaded::Idle,
            loaded_ctx,
            idle_ctx,
            armed: None,
            #[cfg(feature = "protocol-port")]
            park_keeps_lapsed_lend: false,
            #[cfg(feature = "protocol-port")]
            migrate_keeps_the_corpse: false,
            #[cfg(feature = "protocol-port")]
            rt_outranks_every_corpse: false,
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
        self.dying.iter().map(|corpse| &corpse.task)
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

    /// Hand a killed ready task to another CPU instead of keeping it — the
    /// balance path before [`CpuSched::hand_off`] checked the kill bit. It puts
    /// the task in `InTransit`, whose handling rides an `Urgency::Normal` adopt
    /// and therefore waits for the destination's next voluntary pass; the
    /// retirer's own bound is wall clock. Invariant I14 must catch it;
    /// `scenarios::old_migrate_kept_the_corpse` is the gate that proves it
    /// does.
    pub fn set_migrate_keeps_the_corpse(&mut self, keep: bool) {
        self.migrate_keeps_the_corpse = keep;
    }

    /// Give the real-time band *unqualified* precedence over the dying list —
    /// the shape this branch shipped between the two fixes, where `pick` asked
    /// only `rq.has_rt()` and [`DYING_AGE_NS`] did not exist.
    ///
    /// One permanently-RT thread that never parks then holds a CPU's dying list
    /// closed for ever, no sibling can rescue the corpse, and
    /// `scheduler::retire_task`'s wall-clock tripwire panics the kernel from a
    /// legal `Rights::RT` workload. Invariant I14 must catch it;
    /// `scenarios::old_rt_starved_the_corpse` is the gate that proves it does,
    /// and it is the *other* direction of the pair
    /// `scenarios::old_migrate_kept_the_corpse` opens.
    pub fn set_rt_outranks_every_corpse(&mut self, outranks: bool) {
        self.rt_outranks_every_corpse = outranks;
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

    /// Every death goes through here (invariant I11) — and since
    /// `specs/completion-architecture-spec.md` §7.2 there is only one kind: a
    /// task's own `die`, on the stack it unwound. The reap paths this sentence
    /// used to have to name are gone.
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
    /// **A killed task is kept here instead**, and dispatched on this CPU —
    /// see [`CpuSched::begin_dying`]. `InTransit` is the one state whose
    /// handling is not backed by an interrupt: the retire that carries
    /// `Urgency::Preempt` is consumed and dropped by a destination that gets it
    /// ahead of the adopt (`handle_retire`'s home-is-me arm), and the adopt
    /// behind it is `Urgency::Normal`, which by design kicks nobody. So handing
    /// a corpse on trades an unwind that could start in this pass for a wait on
    /// another CPU's next voluntary one. Reading the kill bit here is what stops
    /// a CPU putting a task it knows is dead into that state; what remains is a
    /// kill that lands *after* the adopt was posted, and that case always has a
    /// `Msg::Retire` aimed at the same CPU — whose adopter dispatches it, which
    /// is what makes the chase terminate.
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
            self.begin_dying(task, env, now);
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
            self.begin_dying(task, env, now);
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
        now: Nanos,
    ) {
        let _vruntime = task.share().enter_runnable(env.frontier);
        self.keep_dying(task, now);
    }

    /// The same, for a task that is *already* counted — one taken out of the
    /// run queue, which never left it.
    ///
    /// Both routes end a borrowed RT window: [`ReadyTask::end_lend`] carries
    /// the argument, and this is the one place that has to remember it.
    fn keep_dying(&mut self, mut task: ReadyTask<X>, now: Nanos) {
        task.end_lend();
        self.dying.push_back(Corpse { since: now, task });
    }

    fn handle_wake<H: Hw<Payload = X>, P: PreemptGuard>(
        &mut self,
        key: TaskKey,
        cause: WakeCause,
        env: Env<'_, H, P>,
        now: Nanos,
    ) {
        let Some(entry) = self.parked.remove(&key) else {
            // Not parked here any more: a `Retire` already woke it into the
            // dying list, or its deadline fired first and this wake lost the
            // arbitration CAS. Keys are never reused, so a stale wake is
            // provably about a task that is no longer waiting — a benign no-op
            // (spec §7.6).
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
            self.keep_dying(ready, now);
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
            // stack; it dies at its next safe point (spec §7.6). Consuming the
            // message here is only sound because the sticky kill bit outlives
            // it and *every* safe point honours it, and
            // `specs/completion-architecture-spec.md` §7.2 changed which points
            // those are. **The pick no longer reaps anything** — the argument
            // this comment used to make. What ends the task instead:
            //
            // * `completion::wait` answers `Cancelled` and the caller `?`s it
            //   out, dropping every guard on the way, so the unwind reaches the
            //   thread's own exit;
            // * `WaitTicket::commit` still refuses to park a killed task, which
            //   is what keeps it *running* rather than parked where no wake is
            //   coming;
            // * and `kernel::scheduler::exit_if_killed` at the return to Ring 3
            //   is the backstop for a thread that never parks again, with an
            //   empty kernel stack by construction.
            //
            // The quantum is not the bound any more and saying so would be the
            // same error: what bounds this is the unwind, and
            // `sim::invariants::retire_latency_bound` derives it hop by hop.
            env.hw.need_resched(self.id);
            return;
        }
        // Somewhere else. Re-post the *same* node — legal precisely because
        // this consumer just unlinked it — unless the word names this CPU,
        // which means an `Adopt` is on its way here: re-posting would spin
        // against the producer, and the sticky kill bit already guarantees the
        // adopter *dispatches* it on arrival, into its own dying list, where it
        // unwinds and dies by its own `die`.
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

/// How long the real-time band may defer one corpse's unwind before that
/// corpse outranks it for a single chunk.
///
/// **A killed task is normal-band work whose deferral is bounded, and both
/// halves of that sentence had to be paid for.** Unqualified precedence in
/// either direction is wrong and both were tried:
///
/// * The dying list ahead of `rq` starved a ready real-time task for the whole
///   of an unwind, quantum after quantum, because `preempt_if_due` returned the
///   corpse to `dying` and the pick handed it straight back.
/// * The dying list behind `rq` unconditionally starved the corpse *forever*
///   under one permanently-RT thread that never parks — `Rights::RT` is
///   capability-gated but `soundd` holds it and `SYS_RT_ENTER` has no
///   revocation, so a killed thread of an RT process on that CPU never reaches
///   `Hw::release`, and `scheduler::retire_task`'s tripwire panics the kernel
///   from a legal workload. That is the kernel crashing from userland.
///
/// So the corpse ages. Once its head has stood in the dying list for this long,
/// the next pick takes it ahead of the RT band for one [`DYING_CHUNK_NS`], then
/// [`SchedPass::preempt_if_due`] returns it and the RT band resumes — and the
/// stamp restarts, so the *next* window is another `DYING_AGE_NS` away.
///
/// **One quantum, and the constraint that fixes it is invariant I4.** The
/// widest RT wake-latency bound in the simulator's suite is
/// `IPI_LATENCY_NS + max KernelSection + 2 × RUN_CHUNK_NS` = 2,700,000 ns, and
/// aging adds `DYING_CHUNK_NS` to it — 3,700,000 ns. For "at most one aged
/// chunk per I4 window" to be a fact rather than a hope, this has to be wider
/// than that bound, because the window closes the moment the RT task actually
/// runs and the next aged chunk is one full `DYING_AGE_NS` further on. 10 ms is
/// the scheduler's own unit and clears 3.7 ms by 2.7×.
pub const DYING_AGE_NS: u64 = QUANTUM_NS;

/// What an aged corpse gets when it outranks the real-time band: one tenth of a
/// quantum, and the quantum a pick gives it when the RT band is *empty* is
/// still the full [`QUANTUM_NS`].
///
/// **This is the number invariant I4's bound grows by**, so it is as small as
/// the other side can afford. Under saturated RT an unwind is delivered at one
/// chunk per `DYING_AGE_NS + DYING_CHUNK_NS`, so the corpse's release is
/// stretched by 11× — which is a term of `scheduler::retire_task`'s `GIVE_UP`
/// derivation and the reason that number moved. A larger chunk buys that term
/// back and spends it on RT latency; `soundd` is the process that pays, and 1 ms
/// of added worst-case jitter once per 10 ms is the trade this picks.
pub const DYING_CHUNK_NS: u64 = QUANTUM_NS / 10;

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
            if task.shared().kill_pending() {
                self.cpu.keep_dying(task, self.now);
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
            // **Two numbers, because two consumers ask two questions** — see
            // [`CpuHandle::load`] and [`CpuHandle::surplus`]. Spawn placement
            // wants everything a new task would queue behind, and a corpse
            // mid-unwind is exactly that: it is dispatched ahead of the fair
            // band, so counting `rq` alone made a CPU holding two teardowns look
            // as empty as an idle one — the same blindness `dying_len` closes in
            // the dump. The steal probe wants what this CPU could hand over,
            // which is the fair band and only the fair band; publishing the
            // first number to the second reader sent thieves to CPUs with
            // nothing to give.
            //
            // Both are sampled after the pick, so neither counts the task this
            // CPU is about to run.
            let handle = self.env.cpus.get(self.cpu.id);
            handle.publish_load((self.cpu.rq.len() + self.cpu.dying.len()) as u32);
            handle.publish_surplus(self.cpu.rq.fair_len() as u32);
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
        // The RT arm does not fire against an aged corpse inside its granted
        // chunk — that grant is the whole of the bounded deferral, and the
        // quantum arm above is what ends it.
        //
        // **`serves_rt_band` and not `is_rt`**, which is what makes this arm and
        // `pick` agree about one task. A killed thread that had called
        // `SYS_RT_ENTER` still answers `is_rt()`, because `RtState::release`
        // ends a lend and leaves the permanent flag alone — so this arm exempted
        // it while the pick gated its dying list on `rq.has_rt()` regardless,
        // and it held the CPU for a full quantum against a ready RT sibling.
        let rt_due = self.cpu.rq.has_rt() && !current.serves_rt_band() && !self.cpu.aged_grant;
        let due = self.now >= self.cpu.quantum_end || rt_due;
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
            self.cpu.keep_dying(task, self.now);
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
    /// **It does not jump the RT band, and it is not held off by it for ever
    /// either — the deferral is bounded, and both absolutes were tried.**
    /// `rq.pop_next()` is the only place the RT band is served, so a pick that
    /// emptied `dying` first left a killed normal task holding the CPU against a
    /// ready real-time task for the whole of its unwind, quantum after quantum,
    /// because `preempt_if_due` returns it to `dying` and this pick handed it
    /// straight back with a fresh quantum. That contradicts
    /// `scheduler-core-spec.md` §3 outright.
    ///
    /// Asking only `rq.has_rt()` is the other absolute and it is worse, because
    /// its failure is a kernel panic: one permanently-RT thread that never parks
    /// holds this CPU's dying list closed for ever, no sibling CPU can rescue a
    /// corpse (`hand_off` refuses to migrate a killed task, `pop_surplus` reads
    /// `fair` only), and `scheduler::retire_task`'s tripwire fires. That is
    /// reachable from a legal `Rights::RT` workload — `soundd` holds the right
    /// and `SYS_RT_ENTER` has no revocation — so it is the kernel crashing from
    /// userland.
    ///
    /// So the question asked here is `rq.has_rt()` **unless the head of the
    /// dying list has waited [`DYING_AGE_NS`]**, and an aged corpse is
    /// dispatched for [`DYING_CHUNK_NS`] rather than a full quantum:
    /// `preempt_if_due` takes it back on the RT arm at that boundary and
    /// [`CpuSched::keep_dying`] restamps it, so the RT band gives up at most one
    /// chunk per age window and the unwind is delivered at that rate.
    /// `a_killed_task_does_not_starve_a_ready_rt_task` and
    /// `a_corpse_is_not_starved_for_ever_by_a_spinning_rt_task` are the two
    /// gates, and they are the two directions.
    fn pick(&mut self) {
        if self.cpu.running.is_some() {
            return;
        }
        #[cfg(not(feature = "protocol-port"))]
        let never_aged = false;
        #[cfg(feature = "protocol-port")]
        let never_aged = self.cpu.rt_outranks_every_corpse;
        let aged = !never_aged
            && self
                .cpu
                .dying
                .front()
                .is_some_and(|corpse| self.now >= corpse.since.after(DYING_AGE_NS));
        if !self.cpu.rq.has_rt() || aged {
            if let Some(corpse) = self.cpu.dying.pop_front() {
                let task = corpse.task;
                let key = task.key();
                self.cpu.running = Some(task.dispatch(self.cpu.id, self.now));
                // An aged corpse is borrowing the CPU from the RT band, so it
                // borrows a chunk and not a quantum. With the band empty it is
                // ordinary normal-band work and gets the ordinary quantum.
                self.cpu.aged_grant = self.cpu.rq.has_rt();
                self.cpu.quantum_end = self.now.after(if self.cpu.aged_grant {
                    DYING_CHUNK_NS
                } else {
                    QUANTUM_NS
                });
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
            self.cpu.aged_grant = false;
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
        // **Chosen by surplus and not by load**, which are two numbers because
        // they answer two questions — see [`CpuHandle::surplus`]. The guard is
        // the same inequality [`SchedPass::answer_steal_requests`] enforces
        // (`fair_len() > 1`), read one CPU away: a victim that would refuse the
        // probe is not probed, and the probe is spent on a CPU that can answer
        // it.
        let Some((victim, _)) = (0..self.env.cpus.len())
            .map(|i| CpuId(i as u32))
            .filter(|&cpu| cpu != self.cpu.id)
            .map(|cpu| (cpu, self.env.cpus.get(cpu).surplus()))
            .max_by_key(|&(_, surplus)| surplus)
        else {
            return;
        };
        if self.env.cpus.get(victim).surplus() < 2 {
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
    /// **How much work a task placed here would queue behind**, published for
    /// spawn placement ([`kernel::sched::driver`]'s `placement`). Counts the run
    /// queue *and* the dying list: a corpse mid-unwind is dispatched ahead of
    /// the fair band, so a task placed here waits for it.
    load: AtomicU32,
    /// **How many fair-band tasks this CPU could give away**, published for the
    /// steal probe and for nothing else.
    ///
    /// A separate number from [`Self::load`] because the two questions have
    /// different answers and one of them was being asked with the other's:
    /// [`SchedPass::answer_steal_requests`] hands over `rq.pop_surplus()`, which
    /// reads the *fair* band only, so a corpse and a queued real-time task are
    /// both work that inflates `load` and can never be stolen. A thief choosing
    /// its victim by `load` picks the CPU holding two teardowns, is answered
    /// with nothing, and sleeps — and the probe is one-shot per idle trip, so
    /// the genuinely surplus-holding CPU goes unprobed for a whole idle round.
    surplus: AtomicU32,
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
            surplus: AtomicU32::new(0),
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

    pub fn publish_load(&self, queued: u32) {
        self.load.store(queued, Ordering::Relaxed);
    }

    pub fn surplus(&self) -> u32 {
        self.surplus.load(Ordering::Relaxed)
    }

    pub fn publish_surplus(&self, fair: u32) {
        self.surplus.store(fair, Ordering::Relaxed);
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
/// **`cpu.rs` had no test module at all until the completion work needed one**,
/// and every arm exercised below — the retire's three, the pick's, the balance
/// path's and the adopt's — was reachable only through the simulator, which
/// explores *scenarios* rather than stating what a single arm does. The
/// scheduler migration cost about seventy defects in code whose own suites were
/// green; these are the statements a reader can check one at a time.
///
/// The harness is deliberately the smallest thing that can hold a `CpuSched`:
/// a payload with no address space, an `Hw` that records rather than acts, and
/// one CPU unless a test needs two.
///
/// **A world holding one task cannot state where a disposition put it.** The
/// pick launders every container back into `running`, so "it ended up running"
/// is what a fair-queue route, a dying-list route and a no-op all produce, and
/// three of the four gates in this module were vacuous for exactly that reason
/// until they were made to red under a mutation. A *second* occupant — a task
/// already in the dying list, or an RT task already in the band — is what makes
/// the answer observable, because the pick then takes that one and leaves the
/// one under test where the disposition put it. Every test below that asserts a
/// container says which second occupant it needs and why.
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
        /// `Env::steal`. Off by default because most tests want a CPU's queue
        /// to stay where they put it; the probe's own tests turn it on.
        steal: bool,
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
                steal: false,
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
                    steal: self.steal,
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

        /// Push the `Msg::Wake` for a task whose claim has **already** been
        /// won — `waitq::deliver_wake`'s second half, split from its first.
        ///
        /// A waker is two steps: the claim CAS, then the push. They are not one
        /// instruction and nothing makes them one, so a message posted by
        /// anybody else can land in between — and for a retire, landing in
        /// between is the whole of §7.2(c). The mailbox is FIFO, so a test that
        /// posts the wake first can never observe that order; this is how it
        /// gets to.
        fn post_claimed_wake(
            &self,
            cpu: CpuId,
            shared: &Arc<TaskShared<Msg<TestPayload>>>,
            reason: WakeReason,
        ) {
            let cause = WakeCause::new(reason);
            let slot = shared
                .wake_node()
                .claim()
                .expect("the wake claim admits one poster: node must be free");
            if self.handles.get(cpu).post(
                slot,
                Msg::wake(shared.key(), cause),
                cause.urgency(),
                &NoPreempt,
            ) == Kick::Send
            {
                self.hw.kick(cpu);
            }
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
        assert_eq!(w.cpus[0].dying[0].task.key(), key);
        assert_eq!(shared.state(), TaskState::Ready(C0));
        w.abandon();
    }

    /// The claim is arbitrated and not assumed: a remote waker that got there
    /// first owns a `Msg::Wake` in flight to this same CPU, so the retire
    /// leaves the entry alone. Removing it here would leave the task in no
    /// container at all — never runnable, never reaped — which is §7.2(c).
    ///
    /// **The order has to be driven by hand, and this test did not use to be
    /// able to reach the arm it is named for.** Posting the wake and then the
    /// retire cannot: the mailbox is FIFO, so the wake is drained first,
    /// `parked` is empty by the time `handle_retire` runs, and the whole
    /// `parked.contains_key` branch is skipped — with `Claim::Lost =>
    /// panic!()` staged in it, this test passed. What reaches the arm is the
    /// window a waker really has ([`World::post_claimed_wake`]): the claim CAS
    /// has been won and the push has not happened yet, so the retire is the
    /// first message this CPU sees.
    ///
    /// The first block below is what has the teeth. It reds under
    /// remove-then-convert (the defect §7.2(c) names) and under a retirer that
    /// claims and ignores the answer: both take the entry out of `parked`, and
    /// there is nothing there for the wake to find.
    #[test]
    fn a_retire_that_loses_the_claim_leaves_the_wake_in_flight() {
        let mut w = World::new(1);
        let q = queue();
        let (key, shared) = w.spawn(C0);
        w.run_a_pass(C0);
        w.park_running(C0, &q);

        // A waker wins the claim; its message has not been pushed yet.
        assert_eq!(shared.claim_wake(), Claim::Parked(C0), "the waker owns it");
        crate::retire::begin(&shared).post(&w.handles, &w.hw, &NoPreempt);
        {
            let (cpus, env) = w.split();
            cpus[0].drain(env, NOW);
        }

        assert!(
            w.cpus[0].parked_task(key).is_some(),
            "the retire lost the claim: the entry stays for the wake to find",
        );
        assert_eq!(w.cpus[0].dying_len(), 0, "the retire placed nothing itself");
        assert!(w.cpus[0].rq.is_empty(), "and queued nothing either");

        // Now the wake it lost to lands, and *it* places the task — in the
        // dying list, because the kill bit is already set.
        w.post_claimed_wake(C0, &shared, WakeReason::Woken);
        {
            let (cpus, env) = w.split();
            cpus[0].drain(env, NOW);
        }

        assert!(w.released().is_empty());
        assert_eq!(w.cpus[0].dying.len(), 1, "the in-flight wake placed it");
        assert_eq!(w.cpus[0].dying[0].task.key(), key);
        assert_eq!(shared.state(), TaskState::Ready(C0));
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
        assert_eq!(w.cpus[0].dying[0].task.key(), key);
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
    /// the CPU, it is releasing resources a retirer is blocked on, so its wait
    /// is one pick and not the depth of the fair band. (It *is* the depth of
    /// the dying list — invariant I14's `(1 + peers)` term — which is the
    /// container this jump does not exempt it from.)
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
            cpus[0].keep_dying(task, NOW);
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
    ///
    /// **The second corpse is what makes this test a statement about where the
    /// first one landed.** With one task on the CPU the pick empties whichever
    /// container `preempt_if_due` put it in and hands it straight back, so
    /// "it is running again" and "the fair queue is empty" read the same
    /// whether the kill arm is there or not — deleting that arm left this test
    /// passing. A corpse already queued ahead of it is what the pick takes
    /// instead, and where the expiring one went stays observable.
    #[test]
    fn a_killed_task_that_expires_its_quantum_goes_back_to_the_dying_list() {
        let mut w = World::new(1);
        let (expiring, expiring_shared) = w.spawn(C0);
        w.run_a_pass(C0);
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(expiring));
        expiring_shared.mark_kill();

        // A second teardown, already waiting its turn on this CPU.
        let (queued, queued_shared) = w.spawn(C0);
        queued_shared.mark_kill();
        {
            let (cpus, _env) = w.split();
            let task = cpus[0].rq.remove(queued).expect("ready");
            cpus[0].keep_dying(task, NOW);
        }

        {
            let (cpus, env) = w.split();
            let pass = SchedPass::begin(&mut cpus[0], env, Nanos(NOW.0 + QUANTUM_NS + 1));
            let _ = pass.dispose_none().finish();
        }

        assert!(w.released().is_empty());
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(queued),
            "the corpse that has been waiting longest unwinds next",
        );
        assert_eq!(
            w.cpus[0].dying_len(),
            1,
            "and the one whose quantum expired went back to the dying list",
        );
        assert_eq!(w.cpus[0].dying[0].task.key(), expiring);
        assert!(w.cpus[0].rq.is_empty(), "never through the fair queue");
        w.abandon();
    }

    /// The same statement at the *voluntary* disposition. A killed task that
    /// yields is on its way out too, so it goes back to the dying list and
    /// never into the fair band.
    ///
    /// Read against its involuntary twin above, and gated the same way: with a
    /// single task on the CPU the pick launders the difference, and deleting
    /// `dispose_yield`'s kill arm left the whole suite green. The corpse queued
    /// ahead of it is what keeps the answer visible.
    #[test]
    fn a_killed_task_that_yields_goes_back_to_the_dying_list() {
        let mut w = World::new(1);
        let (yielder, yielder_shared) = w.spawn(C0);
        let (queued, queued_shared) = w.spawn(C0);
        yielder_shared.mark_kill();
        queued_shared.mark_kill();
        {
            let (cpus, _env) = w.split();
            for key in [yielder, queued] {
                let task = cpus[0].rq.remove(key).expect("ready");
                cpus[0].keep_dying(task, NOW);
            }
        }
        w.run_a_pass(C0);
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(yielder));

        {
            let (cpus, env) = w.split();
            let pass = SchedPass::begin(&mut cpus[0], env, Nanos(NOW.0 + 1));
            let _ = pass.dispose_yield().finish();
        }

        assert!(w.released().is_empty());
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(queued),
            "the yield hands the CPU to the corpse that was waiting",
        );
        assert_eq!(
            w.cpus[0].dying_len(),
            1,
            "and the yielder went back to the dying list",
        );
        assert_eq!(w.cpus[0].dying[0].task.key(), yielder);
        assert!(w.cpus[0].rq.is_empty(), "never into the fair queue");
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
    /// like any other task and routes it by its kill bit, which is what makes
    /// the retire chase terminate — for a sharper reason than the reap it
    /// replaces.
    ///
    /// **The routing is asserted before the pick runs, and that is the half
    /// with teeth.** "It ends up running" is equally true of an adopt that put
    /// the corpse in the fair band, so `place` → `enqueue` in `handle_adopt`
    /// used to be invisible to the whole suite. A corpse in the fair band is
    /// ordinary work: it queues behind whatever is there, `answer_steal_requests`
    /// may hand it to another CPU as surplus, and the retirer's bound picks up
    /// the whole depth of the fair band — which is exactly what the dying list
    /// exists to prevent.
    #[test]
    fn an_adopt_of_a_killed_task_dispatches_it_on_arrival() {
        let mut w = World::new(2);
        let (key, shared) = w.spawn(C0);

        {
            let (cpus, env) = w.split();
            let task = cpus[0].rq.remove(key).expect("ready on cpu0");
            task.share().leave_runnable(env.frontier);
            cpus[0].hand_off(task, C1, env, NOW);
        }
        assert_eq!(shared.state(), TaskState::InTransit(C1));

        shared.mark_kill();
        {
            let (cpus, env) = w.split();
            cpus[1].drain(env, NOW);
        }

        assert_eq!(w.cpus[1].dying_len(), 1, "the arriving corpse is placed to unwind");
        assert_eq!(w.cpus[1].dying[0].task.key(), key);
        assert!(
            w.cpus[1].rq.is_empty(),
            "and never into the fair band, where it would be ordinary work",
        );

        w.run_a_pass(C1);

        assert!(w.released().is_empty(), "nothing was discarded in flight");
        assert_eq!(
            w.cpus[1].running().map(|t| t.key()),
            Some(key),
            "and the next pick takes it ahead of the fair band",
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
    /// fair queue because a retirer is blocked on what it holds; a corpse that
    /// has *not* aged does not jump the RT band, because nothing about an
    /// unwind makes it more urgent than real-time work.
    ///
    /// The pass that fires because the RT task is ready must not hand the CPU
    /// straight back to the corpse: `preempt_if_due` takes it off, and the pick
    /// then serves `rq` — which is where the RT band lives — before `dying`.
    ///
    /// **This stages the un-aged case and only that case**, which is the
    /// direction it was written for: the corpse here is killed and preempted
    /// inside one pass, so it has stood in the list for far less than
    /// [`DYING_AGE_NS`] and `pick`'s aging test is false.
    /// `a_corpse_is_not_starved_for_ever_by_a_spinning_rt_task` is the other
    /// direction and stages the aged one. Neither absolute holds, and a doc
    /// here that stated one — "spec §3's 'a ready real-time task always
    /// preempts the normal band' admits no exception for it" — was carrying an
    /// absolute this crate's own gates already disprove.
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
            cpus[0].keep_dying(task, NOW);
            let task = cpus[0].rq.remove(second).expect("ready");
            cpus[0].keep_dying(task, NOW);
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

    /// **The other direction of the same law, and the one the first fix
    /// created.** The three tests above say a corpse never starves the RT band.
    /// This one says the RT band never starves the corpse, because unqualified
    /// RT precedence over the dying list ends in a kernel panic:
    /// `scheduler::retire_task` blocks on `Hw::release` behind a tripwire, and
    /// one permanently-RT thread that never parks holds this CPU's dying list
    /// closed for ever. `hand_off` refuses to migrate a killed task and
    /// `pop_surplus` reads `fair` only, so no sibling CPU can rescue it.
    ///
    /// The workload is legal: `Rights::RT` is capability-gated, `soundd` holds
    /// it, and `SYS_RT_ENTER` has no revocation anywhere in the tree.
    ///
    /// So the deferral is bounded, and the bound is measured here rather than
    /// asserted: driven by the CPU's own armed timer, the corpse must get a
    /// [`DYING_CHUNK_NS`] slice at least once per
    /// `DYING_AGE_NS + DYING_CHUNK_NS`, and the RT task must never be kept
    /// waiting longer than one chunk. Both numbers are printed.
    #[test]
    fn a_corpse_is_not_starved_for_ever_by_a_spinning_rt_task() {
        let mut w = World::new(1);
        let (killed, killed_shared) = w.spawn(C0);
        w.run_a_pass(C0);
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(killed));
        killed_shared.mark_kill();

        let (rt, _rt_shared) = w.spawn_rt(C0);
        let mut now = Nanos(NOW.0 + 1);
        w.run_a_pass_at(C0, now);
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(rt),
            "the RT task still takes the CPU on the pass that makes it ready",
        );
        assert_eq!(w.cpus[0].dying_len(), 1, "and the corpse is queued");

        // Follow the armed timer, which is the only thing that takes the CPU
        // away from a task nothing preempts — a real machine does exactly this.
        // Ten age windows is long enough for a starvation to be unmistakable and
        // short enough to stay a unit test.
        let horizon = Nanos(now.0 + 10 * (DYING_AGE_NS + DYING_CHUNK_NS));
        let mut corpse_ns = 0u64;
        let mut rt_ns = 0u64;
        let mut longest_corpse_wait = 0u64;
        let mut longest_rt_wait = 0u64;
        let mut corpse_waiting_since = now;
        let mut rt_waiting_since = None;
        let mut chunks = 0u32;
        while now < horizon {
            let armed = w.cpus[0]
                .armed()
                .expect("a CPU with a running task has its quantum armed");
            let running = w.cpus[0].running().map(|t| t.key());
            let slice = armed.since(now);
            match running {
                Some(k) if k == killed => {
                    corpse_ns += slice;
                    longest_corpse_wait = longest_corpse_wait.max(now.since(corpse_waiting_since));
                    // The RT task is ready and not running for exactly this
                    // slice — the whole of what aging costs invariant I4.
                    rt_waiting_since = Some(now);
                    longest_rt_wait = longest_rt_wait.max(slice);
                }
                Some(k) if k == rt => {
                    rt_ns += slice;
                    corpse_waiting_since = if rt_waiting_since.take().is_some() {
                        now
                    } else {
                        corpse_waiting_since
                    };
                }
                other => panic!("unexpected running task: {other:?}"),
            }
            now = armed;
            w.run_a_pass_at(C0, now);
            if w.cpus[0].running().map(|t| t.key()) == Some(killed) {
                chunks += 1;
            }
        }

        std::eprintln!(
            "aging: corpse ran {corpse_ns} ns in {chunks} chunks, RT ran {rt_ns} ns; \
             longest corpse wait {longest_corpse_wait} ns, longest RT wait {longest_rt_wait} ns",
        );
        assert!(
            chunks >= 9,
            "over ten age windows the corpse was dispatched {chunks} times — a \
             corpse that never runs never reaches `Hw::release`, and the retirer \
             panics the kernel",
        );
        assert!(
            longest_corpse_wait <= DYING_AGE_NS + DYING_CHUNK_NS,
            "the corpse waited {longest_corpse_wait} ns between chunks, and the \
             derived window is {} ns",
            DYING_AGE_NS + DYING_CHUNK_NS,
        );
        assert!(
            longest_rt_wait <= DYING_CHUNK_NS,
            "an aged corpse held the CPU against a ready RT task for \
             {longest_rt_wait} ns, and the grant is {DYING_CHUNK_NS} ns — that is \
             invariant I4's whole new term",
        );
        assert_eq!(
            corpse_ns,
            chunks as u64 * DYING_CHUNK_NS,
            "every aged dispatch is one chunk exactly, never a quantum",
        );
        w.abandon();
    }

    /// The bargain's price, stated as the ratio rather than as a bound: under a
    /// saturated RT band the corpse gets `DYING_CHUNK_NS` out of every
    /// `DYING_AGE_NS + DYING_CHUNK_NS`, so an unwind's wall-clock length is
    /// stretched by that factor and no more.
    ///
    /// It is a separate test because it is the term
    /// `scheduler::retire_task`'s `GIVE_UP` derivation carries, and a change
    /// that quietly widened `DYING_AGE_NS` would leave the gate above green
    /// while making that tripwire wrong.
    #[test]
    fn an_unwind_under_saturated_rt_is_stretched_by_the_age_ratio() {
        let stretch = (DYING_AGE_NS + DYING_CHUNK_NS) / DYING_CHUNK_NS;
        assert_eq!(
            stretch, 11,
            "`retire_task`'s GIVE_UP prices the unwind at {stretch}x its own CPU \
             time; the constants say {}",
            DYING_AGE_NS + DYING_CHUNK_NS,
        );
        assert!(
            DYING_AGE_NS > 3_700_000,
            "an age window narrower than invariant I4's own bound with the aging \
             term in it ({} ns) lets two aged chunks fall inside one I4 window, \
             and 'at most one chunk per window' stops being true",
            200_000 + 500_000 + 2 * 1_000_000 + DYING_CHUNK_NS,
        );
    }

    /// **The grant is a chunk of time and not a pass**, which is the half that
    /// makes it worth anything.
    ///
    /// `preempt_if_due`'s RT arm fires at the next *pass*, and passes are handed
    /// out by every interrupt the machine takes. Without
    /// [`CpuSched::aged_grant`] the first device IRQ inside an aged chunk hands
    /// the CPU straight back to the RT task, the corpse is restamped, and under
    /// an interrupt stream it makes no progress at all — the starvation this
    /// whole mechanism exists to end, arriving one layer down.
    #[test]
    fn an_aged_grant_is_not_undone_by_a_pass_inside_its_chunk() {
        let mut w = World::new(1);
        let (killed, killed_shared) = w.spawn(C0);
        w.run_a_pass(C0);
        killed_shared.mark_kill();
        let (rt, _rt_shared) = w.spawn_rt(C0);
        w.run_a_pass_at(C0, Nanos(NOW.0 + 1));
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(rt));

        // The RT task's quantum expires; the corpse has aged and is dispatched.
        let aged_at = Nanos(NOW.0 + 1 + QUANTUM_NS);
        w.run_a_pass_at(C0, aged_at);
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(killed),
            "the aged corpse takes the CPU ahead of the RT band",
        );
        assert_eq!(
            w.cpus[0].armed(),
            Some(aged_at.after(DYING_CHUNK_NS)),
            "for one chunk, not one quantum",
        );

        // A device interrupt lands halfway through the chunk.
        w.run_a_pass_at(C0, Nanos(aged_at.0 + DYING_CHUNK_NS / 2));
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(killed),
            "and the pass that interrupt buys does not take the grant back",
        );

        // The chunk ends, and the RT band resumes at once.
        w.run_a_pass_at(C0, aged_at.after(DYING_CHUNK_NS));
        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(rt),
            "the grant ends on its own boundary and not a nanosecond later",
        );
        assert_eq!(w.cpus[0].dying_len(), 1, "the corpse is queued again");
        w.abandon();
    }

    /// **A killed thread that holds the RT right unwinds in the normal band**,
    /// and until `serves_rt_band` existed it did not.
    ///
    /// `RtState::release` ends an inherited lend and deliberately leaves the
    /// *permanent* flag alone, so a thread that called `SYS_RT_ENTER` and was
    /// then killed still answers `is_rt()`. `preempt_if_due` asked exactly that
    /// and therefore exempted the corpse, while `pick` gated its dying list on
    /// `rq.has_rt()` whatever it was — two halves of one rule disagreeing about
    /// one task. The corpse held its CPU for a full quantum against a ready
    /// real-time sibling; `soundd` holds the right and a killed `soundd` thread
    /// is exactly this.
    ///
    /// The control is `a_live_fair_task_loses_the_cpu_to_a_ready_rt_task` and
    /// its three siblings above: this asserts the same thing of the one task
    /// that used to be the exception.
    #[test]
    fn a_killed_rt_thread_unwinds_in_the_normal_band() {
        let mut w = World::new(1);
        let (killed, killed_shared) = w.spawn_rt(C0);
        w.run_a_pass(C0);
        assert_eq!(w.cpus[0].running().map(|t| t.key()), Some(killed));
        assert!(
            w.cpus[0].running().expect("running").is_rt(),
            "it holds the RT right, and being killed does not take it back",
        );
        killed_shared.mark_kill();

        let (rt, _rt_shared) = w.spawn_rt(C0);
        w.run_a_pass_at(C0, Nanos(NOW.0 + 1));

        assert_eq!(
            w.cpus[0].running().map(|t| t.key()),
            Some(rt),
            "the corpse is unwinding, not doing real-time work, so the sibling \
             that is doing real-time work gets the CPU at the next pass",
        );
        assert_eq!(w.cpus[0].dying_len(), 1, "and the corpse waits its age out");
        assert!(
            w.cpus[0].dying[0].task.is_rt(),
            "with its right intact — this is about the band it competes in, not \
             about revoking anything",
        );
        w.abandon();
    }

    /// **A corpse is not stealable surplus**, and the probe now asks the
    /// question it means.
    ///
    /// `finish_inner` publishes two numbers because two readers ask two
    /// questions — see [`CpuHandle::load`] and [`CpuHandle::surplus`]. Reading
    /// the placement number for the steal probe sent the thief to the CPU
    /// holding the most *work*, which a CPU deep in two teardowns is, and
    /// `answer_steal_requests` then had nothing to give it: `pop_surplus` reads
    /// the fair band only. The probe is one-shot per idle trip and a sleeping
    /// CPU stops its timer, so that miss costs a whole idle round.
    #[test]
    fn a_steal_probe_goes_to_surplus_and_not_to_a_cpu_full_of_corpses() {
        const C2: CpuId = CpuId(2);
        let mut w = World::new(3);
        w.steal = true;

        // cpu1: one live task running, three corpses queued behind it. Three
        // units of work, none of them stealable.
        let mut on_cpu1 = Vec::new();
        for _ in 0..4 {
            on_cpu1.push(w.spawn(C1));
        }
        w.run_a_pass(C1);
        let running_on_cpu1 = w.cpus[1].running().map(|t| t.key()).expect("one was picked");
        for (key, shared) in &on_cpu1 {
            if *key == running_on_cpu1 {
                continue;
            }
            shared.mark_kill();
            let (cpus, _env) = w.split();
            let task = cpus[1].rq.remove(*key).expect("ready");
            cpus[1].keep_dying(task, NOW);
        }
        w.run_a_pass(C1);

        // cpu2: three ordinary fair tasks, one running and two queued — genuine
        // surplus, and less total work than cpu1 is holding.
        for _ in 0..3 {
            w.spawn(C2);
        }
        w.run_a_pass(C2);

        // cpu0 is idle, and its pass posts the one probe it gets.
        w.run_a_pass(C0);
        // The victim answers from surplus at its next pass, and the answer is an
        // `Adopt` cpu0 takes at its own.
        w.run_a_pass_at(C1, Nanos(NOW.0 + 1));
        w.run_a_pass_at(C2, Nanos(NOW.0 + 1));
        w.run_a_pass_at(C0, Nanos(NOW.0 + 2));

        assert!(
            w.cpus[0].running().is_some(),
            "the idle CPU probed a CPU that could answer it, and got work",
        );
        assert_eq!(
            w.cpus[2].rq.fair_len(),
            1,
            "and the task came out of cpu2's surplus",
        );
        assert_eq!(
            w.cpus[1].dying_len(),
            3,
            "while cpu1's corpses stayed exactly where they were",
        );

        // Read last, so the assertion that fails on a tree publishing one
        // number for both readers is the *behaviour* above and not this.
        assert_eq!(w.handles.get(C1).load(), 3, "cpu1 is holding three corpses");
        assert_eq!(w.handles.get(C1).surplus(), 0, "and can give away none of it");
        assert_eq!(w.handles.get(C2).load(), 1, "cpu2 gave one of its two away");
        assert_eq!(w.handles.get(C2).surplus(), 1);
        w.abandon();
    }
}

/// The pass-cost recorder's own arms, which need the `check` build the
/// recorder only exists in — a separate module for that reason and no other.
#[cfg(all(test, feature = "check"))]
mod pass_cost_tests {
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
