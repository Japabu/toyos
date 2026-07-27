//! Task identity, the rendezvous state word, and the CAS protocol every
//! wake, timeout and retire arbitrates through (spec §5.3, §8.2, §8.3).
//!
//! Ownership truth is the linear `Task` value and the container it sits in;
//! those land with the per-CPU machine at migration Stage 4. What lives here
//! is the *runtime shadow* remote CPUs need: one atomic word plus the two
//! embedded mailbox nodes, inside an `Arc<TaskShared>` that outlives the
//! task's death so a late message about a dead task is a benign no-op.

use crate::hw::{CpuId, Nanos};
use crate::mailbox::MailboxNode;
use crate::sync::{AtomicBool, AtomicU64, Ordering};

/// Monotonic, never reused. Stale messages keyed by `TaskKey` are provably
/// about a dead task and are benign no-ops.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TaskKey(pub u64);

/// What the embedding world attaches to every task: the saved-context type
/// plus environment-owned per-task data. The kernel supplies kernel stack,
/// address-space Arc and fs_base; the simulator supplies mock payloads whose
/// refcounts the invariant checkers watch.
pub trait SchedPayload: Sized {
    /// Saved callee context, restored by `Hw::switch`.
    type Ctx: Sized;
}

/// Why a task is being woken, and whether the waker lends it RT priority for
/// a bounded window (spec §8.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WakeCause {
    pub reason: WakeReason,
    pub boost: Option<BoostWindow>,
}

impl WakeCause {
    pub fn new(reason: WakeReason) -> Self {
        Self {
            reason,
            boost: None,
        }
    }

    pub fn boosted(reason: WakeReason, until: Nanos) -> Self {
        Self {
            reason,
            boost: Some(BoostWindow { until }),
        }
    }

    /// RT and boost wakes must preempt the target promptly; ordinary wakes
    /// ride the target's next safe point (spec §7.3).
    pub fn urgency(&self) -> crate::mailbox::Urgency {
        match self.boost {
            Some(_) => crate::mailbox::Urgency::Preempt,
            None => crate::mailbox::Urgency::Normal,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WakeReason {
    /// The waited-for condition became true.
    Woken,
    /// The parked task's deadline fired on its home CPU.
    Timeout,
}

/// Inherited RT expires on a wall-clock bound, not on "until the next block",
/// so a boosted client that spins cannot keep RT forever (spec §8.5, I9).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoostWindow {
    pub until: Nanos,
}

/// What a parked task is waiting for — accounting only; the scheduler itself
/// knows nothing about event sources (spec §8.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitClass {
    Io,
    Futex,
    Pipe,
    Ipc,
    Other,
}

/// Distinguishes one `prepare_wait` from the next on the same task, so a
/// claim that raced an earlier, already-cancelled registration cannot be
/// mistaken for a claim on the current one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Gen(pub u32);

/// The rendezvous states of §5.3. The CPU in each variant is the task's
/// home — the only CPU allowed to own it as a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Running(CpuId),
    Ready(CpuId),
    /// Registered on a wait queue, not yet parked: the two-phase commit's
    /// first phase (spec §8.1).
    Committing(CpuId, Gen),
    Blocked(CpuId),
    /// A waker won the claim and a `Wake` message is queued to the home CPU.
    WakeQueued(CpuId),
    /// Owned by an unconsumed `Msg::Adopt` on its way to this CPU.
    InTransit(CpuId),
    Dead,
}

const DISC_BITS: u32 = 3;
const DISC_MASK: u64 = (1 << DISC_BITS) - 1;
const CPU_SHIFT: u32 = DISC_BITS;
const CPU_BITS: u32 = 16;
const CPU_MASK: u64 = (1 << CPU_BITS) - 1;
const GEN_SHIFT: u32 = CPU_SHIFT + CPU_BITS;
const GEN_BITS: u32 = 32;
const GEN_MASK: u64 = (1 << GEN_BITS) - 1;

/// Sticky: set by the retirer before it posts, never cleared. Any CPU that
/// adopts the task converts it to a dead task on arrival, which is what makes
/// the retire chase terminate (spec §7.6).
const KILL: u64 = 1 << 62;
/// Sticky: exactly one retirer may post the retire node (spec §7.6).
const RETIRE_QUEUED: u64 = 1 << 63;
const STICKY: u64 = KILL | RETIRE_QUEUED;

const D_RUNNING: u64 = 0;
const D_READY: u64 = 1;
const D_COMMITTING: u64 = 2;
const D_BLOCKED: u64 = 3;
const D_WAKE_QUEUED: u64 = 4;
const D_IN_TRANSIT: u64 = 5;
const D_DEAD: u64 = 6;

fn pack(state: TaskState) -> u64 {
    let (disc, cpu, generation) = match state {
        TaskState::Running(c) => (D_RUNNING, c.0, 0),
        TaskState::Ready(c) => (D_READY, c.0, 0),
        TaskState::Committing(c, g) => (D_COMMITTING, c.0, g.0),
        TaskState::Blocked(c) => (D_BLOCKED, c.0, 0),
        TaskState::WakeQueued(c) => (D_WAKE_QUEUED, c.0, 0),
        TaskState::InTransit(c) => (D_IN_TRANSIT, c.0, 0),
        TaskState::Dead => (D_DEAD, 0, 0),
    };
    assert!(u64::from(cpu) <= CPU_MASK, "cpu id out of range: {cpu}");
    disc | (u64::from(cpu) << CPU_SHIFT) | ((u64::from(generation) & GEN_MASK) << GEN_SHIFT)
}

const GEN_FIELD: u64 = GEN_MASK << GEN_SHIFT;

/// The word `cur` should become when the task moves to `to`: sticky bits are
/// preserved, and so is the commit generation — it is a per-task counter, not
/// a per-state field, so a registration that was cancelled cannot have its
/// number handed out again (which would let a stale claim commit a later
/// wait).
fn retarget(cur: u64, to: TaskState) -> u64 {
    let generation = match to {
        TaskState::Committing(..) => 0,
        _ => cur & GEN_FIELD,
    };
    (cur & STICKY) | generation | pack(to)
}

fn unpack(word: u64) -> TaskState {
    let cpu = CpuId(((word >> CPU_SHIFT) & CPU_MASK) as u32);
    let generation = Gen(((word >> GEN_SHIFT) & GEN_MASK) as u32);
    match word & DISC_MASK {
        D_RUNNING => TaskState::Running(cpu),
        D_READY => TaskState::Ready(cpu),
        D_COMMITTING => TaskState::Committing(cpu, generation),
        D_BLOCKED => TaskState::Blocked(cpu),
        D_WAKE_QUEUED => TaskState::WakeQueued(cpu),
        D_IN_TRANSIT => TaskState::InTransit(cpu),
        D_DEAD => TaskState::Dead,
        other => panic!("corrupt task state word: discriminant {other}"),
    }
}

/// The complete set of legal edges, in one place, mirroring the §5.2
/// transition table. Anything else is a scheduler bug and panics at the
/// transition instead of corrupting the shadow silently — the fail-fast layer
/// that would have preserved crash.md's evidence.
fn legal(from: TaskState, to: TaskState) -> bool {
    use TaskState::*;
    match (from, to) {
        // Dispositions of the running task; the home CPU never changes here.
        (Running(a), Ready(b)) | (Running(a), Committing(b, _)) => a == b,
        (Running(_), Dead) => true,
        // Pick, migrate, reap.
        (Ready(a), Running(b)) => a == b,
        (Ready(_), InTransit(_)) | (Ready(_), Dead) => true,
        // The two-phase wait handshake.
        (Committing(a, _), Running(b))
        | (Committing(a, _), Blocked(b))
        | (Committing(a, _), WakeQueued(b)) => a == b,
        // Wake arbitration and delivery.
        (Blocked(a), WakeQueued(b)) => a == b,
        (WakeQueued(a), Ready(b)) => a == b,
        // A pre-park claim (`Committing → WakeQueued`) posts no message, so
        // the waiter's own commit or cancel resolves it by staying runnable
        // (spec §8.1's `AlreadyWoken`).
        (WakeQueued(a), Running(b)) => a == b,
        (Blocked(_), Dead) | (WakeQueued(_), Dead) => true,
        // Adoption at the far end of a migration.
        (InTransit(a), Ready(b)) => a == b,
        (InTransit(_), Dead) => true,
        _ => false,
    }
}

/// The outcome of a waker's claim (spec §8.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Claim {
    /// The task was parked on `CpuId`: we own the wake and must post
    /// `Msg::Wake` to that CPU.
    Parked(CpuId),
    /// The waiter had registered but not yet parked. Its own commit will
    /// observe the claim and refuse to park — no message needed.
    PrePark,
    /// Somebody else (a local deadline fire, a retire) got there first; this
    /// waiter is no longer waiting. A `wake_one` must try the next one — a
    /// wake may never be satisfied by a corpse.
    Lost,
}

/// The outcome of the second phase of the wait handshake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParkOutcome {
    /// The state word is now `Blocked`; the pass may park the task.
    Parked,
    /// A wake landed between registration and commit. Do not park, do not
    /// switch (spec §8.1).
    AlreadyWoken,
}

/// The rendezvous word plus the embedded nodes every remote effect rides on.
///
/// Generic over the mailbox message type so the primitives stay free of the
/// task payload; Stage 4 pins `M = Msg<X>`.
pub struct TaskShared<M> {
    key: TaskKey,
    /// `{discriminant, cpu, commit generation}` plus the sticky KILL and
    /// RETIRE_QUEUED bits.
    state: AtomicU64,
    /// ≤1 in flight, guaranteed by the `Blocked → WakeQueued` claim CAS.
    wake_node: MailboxNode<M>,
    /// ≤1 in flight, guaranteed by the sticky RETIRE_QUEUED bit.
    retire_node: MailboxNode<M>,
    /// Membership in at most one wait queue (multi-wait is io_uring's job).
    /// The queue holds the `Arc`; this flag is the fail-fast check that a
    /// task never registers on two queues.
    waiting: AtomicBool,
}

impl<M> TaskShared<M> {
    pub fn new(key: TaskKey, state: TaskState) -> Self {
        Self {
            key,
            state: AtomicU64::new(pack(state)),
            wake_node: MailboxNode::new(),
            retire_node: MailboxNode::new(),
            waiting: AtomicBool::new(false),
        }
    }

    pub fn key(&self) -> TaskKey {
        self.key
    }

    pub fn wake_node(&self) -> &MailboxNode<M> {
        &self.wake_node
    }

    pub fn retire_node(&self) -> &MailboxNode<M> {
        &self.retire_node
    }

    pub fn state(&self) -> TaskState {
        unpack(self.state.load(Ordering::Acquire))
    }

    pub fn kill_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) & KILL != 0
    }

    pub fn retire_queued(&self) -> bool {
        self.state.load(Ordering::Acquire) & RETIRE_QUEUED != 0
    }

    /// Move the word from `from` to `to`, preserving the sticky bits.
    /// `false` means the word was no longer `from` — the caller lost a race
    /// and must re-read.
    pub fn transition(&self, from: TaskState, to: TaskState) -> bool {
        assert!(legal(from, to), "illegal task transition {from:?} -> {to:?}");
        let mut cur = self.state.load(Ordering::Acquire);
        loop {
            if unpack(cur) != from {
                return false;
            }
            let next = retarget(cur, to);
            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Phase 1 of the wait handshake: `Running(cpu) → Committing(cpu, gen)`.
    /// The generation advances on every registration, so a claim that raced
    /// an earlier registration cannot commit this one.
    pub fn begin_commit(&self, cpu: CpuId) -> Gen {
        let mut cur = self.state.load(Ordering::Acquire);
        loop {
            assert_eq!(
                unpack(cur),
                TaskState::Running(cpu),
                "prepare_wait outside the running task's own CPU",
            );
            let generation = Gen((((cur >> GEN_SHIFT) & GEN_MASK) as u32).wrapping_add(1));
            let next = retarget(cur, TaskState::Committing(cpu, generation));
            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return generation,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Phase 2: park if no waker claimed us in between (spec §8.1).
    pub fn commit_park(&self, cpu: CpuId, generation: Gen) -> ParkOutcome {
        if self.transition(
            TaskState::Committing(cpu, generation),
            TaskState::Blocked(cpu),
        ) {
            return ParkOutcome::Parked;
        }
        self.recover_from_claim(cpu)
    }

    /// The condition became true before we parked: unwind phase 1.
    /// `AlreadyWoken` means a waker claimed the registration first — the
    /// caller must treat the wait as satisfied rather than retry.
    pub fn cancel_commit(&self, cpu: CpuId, generation: Gen) -> ParkOutcome {
        if self.transition(
            TaskState::Committing(cpu, generation),
            TaskState::Running(cpu),
        ) {
            return ParkOutcome::Parked;
        }
        self.recover_from_claim(cpu)
    }

    /// The only way to lose a `Committing` transition is a waker's
    /// `Committing → WakeQueued` claim, and that waker posted no message, so
    /// the state word is ours to put back.
    fn recover_from_claim(&self, cpu: CpuId) -> ParkOutcome {
        let recovered = self.transition(TaskState::WakeQueued(cpu), TaskState::Running(cpu));
        assert!(
            recovered,
            "commit lost to something other than a pre-park claim: {:?}",
            self.state(),
        );
        ParkOutcome::AlreadyWoken
    }

    /// The one arbitration point every wake goes through — remote wakers,
    /// local deadline fires, join, device ISR tails (spec §8.2). There is no
    /// second path.
    pub fn claim_wake(&self) -> Claim {
        loop {
            match self.state() {
                TaskState::Blocked(cpu) => {
                    if self.transition(TaskState::Blocked(cpu), TaskState::WakeQueued(cpu)) {
                        return Claim::Parked(cpu);
                    }
                }
                TaskState::Committing(cpu, generation) => {
                    if self.transition(
                        TaskState::Committing(cpu, generation),
                        TaskState::WakeQueued(cpu),
                    ) {
                        return Claim::PrePark;
                    }
                }
                _ => return Claim::Lost,
            }
        }
    }

    /// The home CPU handling `Msg::Wake`: `WakeQueued(cpu) → Ready(cpu)`.
    pub fn finish_wake(&self, cpu: CpuId) -> bool {
        self.transition(TaskState::WakeQueued(cpu), TaskState::Ready(cpu))
    }

    /// Wait-queue membership, one queue at a time. `false` means the task is
    /// already registered somewhere — a caller bug.
    pub fn set_waiting(&self) -> bool {
        !self.waiting.swap(true, Ordering::AcqRel)
    }

    pub fn clear_waiting(&self) {
        self.waiting.store(false, Ordering::Release);
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    /// Sticky KILL + RETIRE_QUEUED. `false` means a retire is already queued
    /// for this task (spec §7.6: exactly one retirer exists, so the caller
    /// fails fast).
    pub(crate) fn claim_retire(&self) -> bool {
        let prev = self.state.fetch_or(KILL | RETIRE_QUEUED, Ordering::AcqRel);
        prev & RETIRE_QUEUED == 0
    }

    /// Mark the task killed without queuing a retire — the panic-recovery
    /// path, which abandons the task instead of retiring it.
    pub fn mark_kill(&self) {
        self.state.fetch_or(KILL, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Shared = TaskShared<u32>;

    const C0: CpuId = CpuId(0);
    const C1: CpuId = CpuId(1);

    fn running(cpu: CpuId) -> Shared {
        Shared::new(TaskKey(1), TaskState::Running(cpu))
    }

    #[test]
    fn word_roundtrips_every_state() {
        for state in [
            TaskState::Running(C1),
            TaskState::Ready(C1),
            TaskState::Committing(C1, Gen(7)),
            TaskState::Blocked(C1),
            TaskState::WakeQueued(C1),
            TaskState::InTransit(CpuId(65535)),
            TaskState::Dead,
        ] {
            assert_eq!(unpack(pack(state)), state);
        }
    }

    #[test]
    fn sticky_bits_survive_transitions() {
        let s = running(C0);
        assert!(s.claim_retire());
        let generation = s.begin_commit(C0);
        assert_eq!(s.state(), TaskState::Committing(C0, generation));
        assert!(s.kill_pending() && s.retire_queued());
        assert_eq!(s.commit_park(C0, generation), ParkOutcome::Parked);
        assert_eq!(s.state(), TaskState::Blocked(C0));
        assert!(s.kill_pending() && s.retire_queued());
    }

    #[test]
    fn a_second_retirer_is_refused() {
        let s = running(C0);
        assert!(s.claim_retire());
        assert!(!s.claim_retire(), "single-retirer is a kernel invariant");
    }

    #[test]
    fn park_then_wake_is_the_ordinary_path() {
        let s = running(C0);
        let generation = s.begin_commit(C0);
        assert_eq!(s.commit_park(C0, generation), ParkOutcome::Parked);
        assert_eq!(s.claim_wake(), Claim::Parked(C0));
        assert_eq!(s.state(), TaskState::WakeQueued(C0));
        assert!(s.finish_wake(C0));
        assert_eq!(s.state(), TaskState::Ready(C0));
    }

    #[test]
    fn a_wake_between_registration_and_commit_refuses_the_park() {
        let s = running(C0);
        let generation = s.begin_commit(C0);
        assert_eq!(s.claim_wake(), Claim::PrePark);
        assert_eq!(s.commit_park(C0, generation), ParkOutcome::AlreadyWoken);
        assert_eq!(s.state(), TaskState::Running(C0), "no switch, keep running");
    }

    #[test]
    fn cancel_reports_a_claim_it_lost() {
        let s = running(C0);
        let generation = s.begin_commit(C0);
        assert_eq!(s.cancel_commit(C0, generation), ParkOutcome::Parked);
        assert_eq!(s.state(), TaskState::Running(C0));

        let generation = s.begin_commit(C0);
        assert_eq!(s.claim_wake(), Claim::PrePark);
        assert_eq!(s.cancel_commit(C0, generation), ParkOutcome::AlreadyWoken);
        assert_eq!(s.state(), TaskState::Running(C0));
    }

    #[test]
    fn a_stale_generation_cannot_park_the_task() {
        let s = running(C0);
        let stale = s.begin_commit(C0);
        assert_eq!(s.cancel_commit(C0, stale), ParkOutcome::Parked);
        let fresh = s.begin_commit(C0);
        assert_ne!(stale, fresh);
        assert!(!s.transition(TaskState::Committing(C0, stale), TaskState::Blocked(C0)));
        assert_eq!(s.state(), TaskState::Committing(C0, fresh));
    }

    #[test]
    fn claims_on_anything_but_a_waiter_are_lost() {
        let s = running(C0);
        assert_eq!(s.claim_wake(), Claim::Lost, "running");
        let generation = s.begin_commit(C0);
        assert_eq!(s.commit_park(C0, generation), ParkOutcome::Parked);
        assert_eq!(s.claim_wake(), Claim::Parked(C0));
        assert_eq!(s.claim_wake(), Claim::Lost, "already claimed");
    }

    #[test]
    #[should_panic(expected = "illegal task transition")]
    fn an_edge_outside_the_table_panics() {
        let s = running(C0);
        s.transition(TaskState::Running(C0), TaskState::Blocked(C0));
    }

    #[test]
    fn wait_membership_is_single_queue() {
        let s = running(C0);
        assert!(s.set_waiting());
        assert!(!s.set_waiting(), "a task waits on at most one queue");
        s.clear_waiting();
        assert!(s.set_waiting());
    }
}
