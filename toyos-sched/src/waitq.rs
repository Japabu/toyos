//! The one wait/wake primitive — spec §8.
//!
//! Every waitable kernel object (pipe end, futex bucket, listener, io_uring
//! CQ, driver queue) owns a [`WaitQueue`]. Every blocking site uses the same
//! two-phase commit, and **every** wake in the system — remote waker, local
//! deadline fire, join, device ISR tail — terminates in the single claim CAS
//! of [`crate::task::TaskShared::claim_wake`]. There is no second path, so
//! the five lost-wake windows (B3) have no place left to live.
//!
//! ```text
//! loop {
//!     if let Some(n) = pipe.try_read(buf) { return n; }
//!     let t = pipe.readers.prepare_wait(&cur);
//!     if pipe.has_data() { t.cancel(); continue; }   // closes the TOCTOU
//!     match block_on(t, deadline) { Woken | Timeout => continue }
//! }
//! ```
//!
//! Two deliberate departures from the spec sketch, both to keep `unsafe`
//! confined to `mailbox.rs`:
//!
//! * The queue's list is behind a [`LeafLock`] the *environment* supplies
//!   (kernel: the IRQ-off leaf lock of §8.1; loom: a `loom::sync::Mutex`),
//!   instead of a raw lock implemented here. The core stays sans-IO and loom
//!   gets to model the real critical sections.
//! * Waiters are held as `Arc<TaskShared>` in a `VecDeque` inside that lock
//!   rather than through an embedded `wait_node` link. `TaskShared.waiting`
//!   still enforces the one-queue-per-task rule; what is deferred is the
//!   allocation-free intrusive list, which needs raw links (see the report
//!   for the Stage 5 note).

use alloc::collections::VecDeque;
use core::marker::PhantomData;

use crate::cpu::CpuHandles;
use crate::hw::{CpuId, Kicker};
use crate::mailbox::{Kick, PreemptGuard, SchedMsg};
use crate::sync::Arc;
use crate::task::{Claim, Gen, ParkOutcome, TaskShared, WaitClass, WakeCause};

pub use crate::sync::LeafLock;

/// The registered waiters, in registration order.
pub struct WaitList<M> {
    waiters: VecDeque<Arc<TaskShared<M>>>,
}

impl<M> WaitList<M> {
    pub fn new() -> Self {
        Self {
            waiters: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.waiters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }
}

impl<M> Default for WaitList<M> {
    fn default() -> Self {
        Self::new()
    }
}

/// The running task, as the wait path sees it. Stage 4 hands this out from
/// `RunningTask`, which is why `prepare_wait` cannot be called for anybody
/// else's task.
pub struct CurrentTask<'a, M> {
    shared: &'a Arc<TaskShared<M>>,
    cpu: CpuId,
}

impl<'a, M> CurrentTask<'a, M> {
    pub fn new(shared: &'a Arc<TaskShared<M>>, cpu: CpuId) -> Self {
        Self { shared, cpu }
    }

    pub fn cpu(&self) -> CpuId {
        self.cpu
    }
}

pub struct WaitQueue<M, L> {
    class: WaitClass,
    list: L,
    _msg: PhantomData<fn() -> M>,
}

impl<M: SchedMsg, L: LeafLock<WaitList<M>>> WaitQueue<M, L> {
    pub fn new(class: WaitClass, list: L) -> Self {
        Self {
            class,
            list,
            _msg: PhantomData,
        }
    }

    pub fn class(&self) -> WaitClass {
        self.class
    }

    pub fn len(&self) -> usize {
        self.list.with(|l| l.len())
    }

    pub fn is_empty(&self) -> bool {
        self.list.with(|l| l.is_empty())
    }

    /// Phase 1: register the running task and move its word to
    /// `Committing(gen)`. Registering before re-checking the condition is
    /// what closes the check-then-block window; the ticket must then be
    /// cancelled or committed.
    #[must_use = "a wait ticket must be committed or cancelled"]
    pub fn prepare_wait<'q>(&'q self, cur: &CurrentTask<'_, M>) -> WaitTicket<'q, M, L> {
        assert!(
            cur.shared.set_waiting(),
            "a task waits on at most one queue",
        );
        let generation = cur.shared.begin_commit(cur.cpu);
        self.list.with(|l| l.waiters.push_back(cur.shared.clone()));
        WaitTicket {
            queue: self,
            shared: cur.shared.clone(),
            cpu: cur.cpu,
            generation,
            armed: true,
            _not_send: PhantomData,
        }
    }

    /// Wake the first waiter that is still waiting. Returns how many wakes
    /// were delivered (0 or 1).
    ///
    /// The retry on [`Claim::Lost`] is load-bearing: without it, a `wake_one`
    /// racing a waiter's timeout would be consumed by the corpse and strand
    /// the next waiter forever — the futex-storm shape (spec §8.2).
    pub fn wake_one(
        &self,
        cause: WakeCause,
        cpus: &CpuHandles<M>,
        kicker: &impl Kicker,
        preempt: &impl PreemptGuard,
    ) -> usize {
        loop {
            let Some(shared) = self.list.with(|l| l.waiters.pop_front()) else {
                return 0;
            };
            shared.clear_waiting();
            match shared.claim_wake() {
                Claim::Parked(cpu) => {
                    deliver_wake(&shared, cpu, cause, cpus, kicker, preempt);
                    return 1;
                }
                // The waiter has not parked yet; its own commit observes the
                // claim and refuses to park. No message needed.
                Claim::PrePark => return 1,
                Claim::Lost => continue,
            }
        }
    }

    /// Wake every current waiter; returns how many claims were won. Used by
    /// the audio path's boost signal (spec §8.5) and by pipe/listener close.
    pub fn wake_all(
        &self,
        cause: WakeCause,
        cpus: &CpuHandles<M>,
        kicker: &impl Kicker,
        preempt: &impl PreemptGuard,
    ) -> usize {
        let mut woken = 0;
        loop {
            let Some(shared) = self.list.with(|l| l.waiters.pop_front()) else {
                return woken;
            };
            shared.clear_waiting();
            match shared.claim_wake() {
                Claim::Parked(cpu) => {
                    deliver_wake(&shared, cpu, cause, cpus, kicker, preempt);
                    woken += 1;
                }
                Claim::PrePark => woken += 1,
                Claim::Lost => {}
            }
        }
    }

    /// Remove a waiter without waking it — the local deadline path, which has
    /// already won the claim on its own CPU (spec §8.3). Idempotent.
    pub fn dequeue(&self, shared: &Arc<TaskShared<M>>) -> bool {
        let key = shared.key();
        let removed = self.list.with(|l| {
            let before = l.waiters.len();
            l.waiters.retain(|w| w.key() != key);
            before != l.waiters.len()
        });
        if removed {
            shared.clear_waiting();
        }
        removed
    }
}

/// Post `Msg::Wake` to the home CPU and honour the kick decision. The only
/// producer of wake messages in the system.
fn deliver_wake<M: SchedMsg>(
    shared: &Arc<TaskShared<M>>,
    cpu: CpuId,
    cause: WakeCause,
    cpus: &CpuHandles<M>,
    kicker: &impl Kicker,
    preempt: &impl PreemptGuard,
) {
    // The claim CAS admits exactly one poster, so the node is free (I12).
    let slot = shared
        .wake_node()
        .claim()
        .expect("the wake claim admits one poster: node must be free");
    let handle = cpus.get(cpu);
    if handle.post(slot, M::wake(shared.key(), cause), cause.urgency(), preempt) == Kick::Send {
        kicker.kick(cpu);
    }
}

/// join / waitpid / sleep and the local deadline fire: the same claim CAS,
/// without a queue. `true` if this caller owns the wake.
pub fn wake_direct<M: SchedMsg>(
    shared: &Arc<TaskShared<M>>,
    cause: WakeCause,
    cpus: &CpuHandles<M>,
    kicker: &impl Kicker,
    preempt: &impl PreemptGuard,
) -> bool {
    match shared.claim_wake() {
        Claim::Parked(cpu) => {
            deliver_wake(shared, cpu, cause, cpus, kicker, preempt);
            true
        }
        Claim::PrePark => true,
        Claim::Lost => false,
    }
}

/// Registered on a queue, not yet parked. `!Send` (the registration belongs
/// to the CPU that made it) and drop-bombed: exactly one of
/// [`WaitTicket::cancel`] and [`WaitTicket::commit`] must consume it, so
/// "registered but neither parked nor cancelled" — a leaked waiter — cannot
/// be reached by forgetting a branch.
#[must_use = "a wait ticket must be committed or cancelled"]
pub struct WaitTicket<'q, M: SchedMsg, L: LeafLock<WaitList<M>>> {
    queue: &'q WaitQueue<M, L>,
    shared: Arc<TaskShared<M>>,
    cpu: CpuId,
    generation: Gen,
    /// Disarmed by `cancel`/`commit`; still armed at drop means a
    /// registration was abandoned.
    armed: bool,
    _not_send: PhantomData<*mut ()>,
}

/// The result of cancelling a registration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cancelled {
    /// The registration was withdrawn; the task keeps running.
    Clean,
    /// A waker had already claimed it. The wait counts as satisfied — the
    /// caller must not wait again for the same event.
    AlreadyWoken,
}

/// The result of committing a registration.
#[must_use]
pub enum Commit<'q, M: SchedMsg, L: LeafLock<WaitList<M>>> {
    /// The state word is `Blocked`; the pass may park the task with the
    /// ticket, and must clear the registration with [`Registration::finish`]
    /// once the task runs again.
    Parked(CommittedTicket<M>, Registration<'q, M, L>),
    /// A wake landed between registration and commit: do not park, do not
    /// switch (spec §8.1).
    AlreadyWoken,
}

/// A live registration on a queue, outstanding while its task is parked.
///
/// It exists because a *timeout* leaves the waiter's node behind: the local
/// deadline fire wins the same claim CAS a waker would, and the core has no
/// idea which queue the task was on — nor should it, since the scheduler
/// knows only tasks, tickets and causes (spec §8.1).
///
/// A leftover node is not merely untidy. Once the task parks on a *second*
/// queue, a `wake_one` on the first would find it `Blocked` and claim it:
/// that queue's wake is consumed by a waiter that is no longer waiting for it
/// — the "wake satisfied by a corpse" failure §8.2 exists to prevent, one
/// level up. Holding this guard across the block, on the task's own stack,
/// makes the cleanup structural instead of one more per-source recheck.
#[must_use = "a registration must be finished once the task runs again"]
pub struct Registration<'q, M: SchedMsg, L: LeafLock<WaitList<M>>> {
    queue: &'q WaitQueue<M, L>,
    shared: Arc<TaskShared<M>>,
    armed: bool,
    _not_send: PhantomData<*mut ()>,
}

impl<M: SchedMsg, L: LeafLock<WaitList<M>>> Registration<'_, M, L> {
    /// Called by the blocking site once its task is running again, whatever
    /// woke it. Idempotent against the ordinary wake path, which dequeued the
    /// waiter when it claimed it.
    pub fn finish(mut self) {
        self.armed = false;
        self.queue.dequeue(&self.shared);
    }
}

impl<M: SchedMsg, L: LeafLock<WaitList<M>>> Drop for Registration<'_, M, L> {
    fn drop(&mut self) {
        assert!(
            !self.armed,
            "wait registration dropped: it must be finished once the task runs",
        );
    }
}

/// Proof that the task's word is `Blocked` and its registration is live —
/// what `SchedPass::dispose_block` consumes at Stage 4.
pub struct CommittedTicket<M> {
    shared: Arc<TaskShared<M>>,
    cpu: CpuId,
    class: WaitClass,
}

impl<M> CommittedTicket<M> {
    pub fn shared(&self) -> &Arc<TaskShared<M>> {
        &self.shared
    }

    pub fn cpu(&self) -> CpuId {
        self.cpu
    }

    pub fn class(&self) -> WaitClass {
        self.class
    }
}

impl<'q, M: SchedMsg, L: LeafLock<WaitList<M>>> WaitTicket<'q, M, L> {
    /// The condition became true after registering: withdraw.
    pub fn cancel(mut self) -> Cancelled {
        self.armed = false;
        self.queue.dequeue(&self.shared);
        match self.shared.cancel_commit(self.cpu, self.generation) {
            ParkOutcome::Parked => Cancelled::Clean,
            ParkOutcome::AlreadyWoken => Cancelled::AlreadyWoken,
        }
    }

    /// Phase 2, run by the blocking pass.
    pub fn commit(mut self) -> Commit<'q, M, L> {
        self.armed = false;
        match self.shared.commit_park(self.cpu, self.generation) {
            ParkOutcome::Parked => Commit::Parked(
                CommittedTicket {
                    shared: self.shared.clone(),
                    cpu: self.cpu,
                    class: self.queue.class(),
                },
                Registration {
                    queue: self.queue,
                    shared: self.shared.clone(),
                    armed: true,
                    _not_send: PhantomData,
                },
            ),
            ParkOutcome::AlreadyWoken => {
                // Every claim path pops the waiter first; this only tidies up
                // after a claim that did not (none today, and cheap insurance
                // against a future one).
                self.queue.dequeue(&self.shared);
                Commit::AlreadyWoken
            }
        }
    }
}

impl<M: SchedMsg, L: LeafLock<WaitList<M>>> Drop for WaitTicket<'_, M, L> {
    fn drop(&mut self) {
        assert!(
            !self.armed,
            "wait ticket dropped: it must be committed or cancelled",
        );
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::cpu::{CpuHandle, CpuHandles};
    use crate::mailbox::{mailbox, MailboxConsumer, MailboxNode, NoPreempt};
    use crate::task::{TaskKey, TaskState, WakeReason};
    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::Mutex;

    #[derive(Debug, PartialEq, Eq)]
    enum Msg {
        Wake(TaskKey, WakeReason),
        Retire(TaskKey),
    }

    impl SchedMsg for Msg {
        fn wake(key: TaskKey, cause: WakeCause) -> Self {
            Msg::Wake(key, cause.reason)
        }
        fn retire(shared: Arc<TaskShared<Self>>) -> Self {
            Msg::Retire(shared.key())
        }
    }

    struct StdLock<T>(Mutex<T>);
    impl<T: Send> LeafLock<T> for StdLock<T> {
        fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
            f(&mut self.0.lock().unwrap())
        }
    }

    #[derive(Default)]
    struct Kicks(Mutex<Vec<CpuId>>);
    impl Kicker for Kicks {
        fn kick(&self, target: CpuId) {
            self.0.lock().unwrap().push(target);
        }
    }

    impl Kicks {
        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    const C0: CpuId = CpuId(0);

    fn queue() -> WaitQueue<Msg, StdLock<WaitList<Msg>>> {
        WaitQueue::new(WaitClass::Pipe, StdLock(Mutex::new(WaitList::new())))
    }

    fn task(key: u64) -> Arc<TaskShared<Msg>> {
        Arc::new(TaskShared::new(TaskKey(key), TaskState::Running(C0)))
    }

    fn cpus() -> (CpuHandles<Msg>, MailboxConsumer<Msg>) {
        let (tx, rx) = mailbox();
        (CpuHandles::new(vec![CpuHandle::new(C0, tx)]), rx)
    }

    fn woken(cause: WakeReason) -> WakeCause {
        WakeCause::new(cause)
    }

    /// Commit a ticket and keep the registration the blocking site would hold
    /// across its block.
    fn park<'q>(
        ticket: WaitTicket<'q, Msg, StdLock<WaitList<Msg>>>,
    ) -> Registration<'q, Msg, StdLock<WaitList<Msg>>> {
        match ticket.commit() {
            Commit::Parked(_, reg) => reg,
            Commit::AlreadyWoken => panic!("expected the park to commit"),
        }
    }

    #[test]
    fn park_then_wake_delivers_exactly_one_message() {
        let q = queue();
        let (handles, mut rx) = cpus();
        let kicks = Kicks::default();
        let t = task(1);

        let reg = park(q.prepare_wait(&CurrentTask::new(&t, C0)));
        assert_eq!(q.len(), 1);

        assert_eq!(
            q.wake_one(woken(WakeReason::Woken), &handles, &kicks, &NoPreempt),
            1
        );
        assert_eq!(rx.pop(&NoPreempt), Some(Msg::Wake(TaskKey(1), WakeReason::Woken)));
        assert_eq!(rx.pop(&NoPreempt), None);
        assert!(q.is_empty() && !t.is_waiting());
        assert!(t.finish_wake(C0));
        reg.finish();
    }

    #[test]
    fn a_wake_before_the_commit_refuses_the_park_and_sends_nothing() {
        let q = queue();
        let (handles, mut rx) = cpus();
        let kicks = Kicks::default();
        let t = task(1);

        let ticket = q.prepare_wait(&CurrentTask::new(&t, C0));
        assert_eq!(
            q.wake_one(woken(WakeReason::Woken), &handles, &kicks, &NoPreempt),
            1
        );
        assert!(matches!(ticket.commit(), Commit::AlreadyWoken));
        assert_eq!(rx.pop(&NoPreempt), None, "pre-park claims post no message");
        assert_eq!(t.state(), TaskState::Running(C0));
        assert_eq!(kicks.count(), 0);
    }

    #[test]
    fn cancel_reports_a_claim_it_lost() {
        let q = queue();
        let (handles, _rx) = cpus();
        let kicks = Kicks::default();
        let t = task(1);

        let ticket = q.prepare_wait(&CurrentTask::new(&t, C0));
        assert_eq!(ticket.cancel(), Cancelled::Clean);
        assert!(q.is_empty(), "cancel dequeues");

        let ticket = q.prepare_wait(&CurrentTask::new(&t, C0));
        q.wake_one(woken(WakeReason::Woken), &handles, &kicks, &NoPreempt);
        assert_eq!(ticket.cancel(), Cancelled::AlreadyWoken);
        assert_eq!(t.state(), TaskState::Running(C0));
    }

    #[test]
    fn a_wake_is_never_satisfied_by_a_corpse() {
        let q = queue();
        let (handles, mut rx) = cpus();
        let kicks = Kicks::default();
        let dead = task(1);
        let alive = task(2);

        let regs: Vec<_> = [&dead, &alive]
            .into_iter()
            .map(|t| park(q.prepare_wait(&CurrentTask::new(t, C0))))
            .collect();
        // The first waiter's deadline fired: its claim is already spent.
        assert_eq!(dead.claim_wake(), Claim::Parked(C0));
        assert_eq!(
            q.wake_one(woken(WakeReason::Woken), &handles, &kicks, &NoPreempt),
            1,
            "the wake skips the corpse and reaches the live waiter",
        );
        assert_eq!(rx.pop(&NoPreempt), Some(Msg::Wake(TaskKey(2), WakeReason::Woken)));
        for reg in regs {
            reg.finish();
        }
    }

    #[test]
    fn wake_all_claims_every_waiter_and_boosts_preempt() {
        let q = queue();
        let (handles, mut rx) = cpus();
        let kicks = Kicks::default();
        let tasks: Vec<_> = (1..=3).map(task).collect();
        let regs: Vec<_> = tasks
            .iter()
            .map(|t| park(q.prepare_wait(&CurrentTask::new(t, C0))))
            .collect();

        let cause = WakeCause::boosted(WakeReason::Woken, crate::hw::Nanos(1_000));
        assert_eq!(q.wake_all(cause, &handles, &kicks, &NoPreempt), 3);
        for key in 1..=3 {
            assert_eq!(rx.pop(&NoPreempt), Some(Msg::Wake(TaskKey(key), WakeReason::Woken)));
        }
        assert_eq!(rx.pop(&NoPreempt), None);
        assert_eq!(kicks.count(), 3, "boost wakes always kick");
        assert!(q.is_empty());
        for reg in regs {
            reg.finish();
        }
    }

    #[test]
    fn the_timeout_path_dequeues_without_a_message() {
        let q = queue();
        let t = task(1);
        let reg = park(q.prepare_wait(&CurrentTask::new(&t, C0)));

        // Local fire: claim on the owning CPU, then leave the queue.
        assert_eq!(t.claim_wake(), Claim::Parked(C0));
        assert!(q.dequeue(&t));
        assert!(!q.dequeue(&t), "idempotent");
        assert!(q.is_empty() && !t.is_waiting());
        reg.finish();
    }

    #[test]
    fn wake_direct_skips_the_queue() {
        let (handles, mut rx) = cpus();
        let kicks = Kicks::default();
        let q = queue();
        let t = task(9);
        let reg = park(q.prepare_wait(&CurrentTask::new(&t, C0)));

        assert!(wake_direct(
            &t,
            woken(WakeReason::Woken),
            &handles,
            &kicks,
            &NoPreempt
        ));
        assert_eq!(rx.pop(&NoPreempt), Some(Msg::Wake(TaskKey(9), WakeReason::Woken)));
        assert!(!wake_direct(
            &t,
            woken(WakeReason::Woken),
            &handles,
            &kicks,
            &NoPreempt
        ));
        reg.finish();
    }

    #[test]
    #[should_panic(expected = "wait ticket dropped")]
    fn dropping_a_ticket_is_loud() {
        let q = queue();
        let t = task(1);
        let _ = q.prepare_wait(&CurrentTask::new(&t, C0));
    }

    #[test]
    fn one_wake_node_serves_the_whole_lifecycle() {
        // Re-parking after a delivered wake reuses the same embedded node,
        // which is only legal because the consumer released it (I12).
        let q = queue();
        let (handles, mut rx) = cpus();
        let kicks = Kicks::default();
        let t = task(1);
        let node: *const MailboxNode<Msg> = t.wake_node();

        for _ in 0..3 {
            let reg = park(q.prepare_wait(&CurrentTask::new(&t, C0)));
            assert_eq!(
                q.wake_one(woken(WakeReason::Woken), &handles, &kicks, &NoPreempt),
                1
            );
            assert!(t.wake_node().in_flight());
            assert_eq!(rx.pop(&NoPreempt), Some(Msg::Wake(TaskKey(1), WakeReason::Woken)));
            assert!(!t.wake_node().in_flight(), "released on consume");
            assert!(t.finish_wake(C0));
            assert!(t.transition(TaskState::Ready(C0), TaskState::Running(C0)));
            reg.finish();
        }
        assert_eq!(node, t.wake_node() as *const _, "the node never moved");
    }
}
