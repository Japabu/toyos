//! The per-CPU run queue — spec §9.2.
//!
//! Two bands: an RT FIFO drained first, and a fair band ordered by
//! `(vruntime, insertion sequence)`. True EEVDF virtual-deadline ordering is a
//! later, sim-gated, `queue.rs`/`fair.rs`-only change (spec §9.1).
//!
//! The queue owns [`ReadyTask`] values. A task in a queue is therefore *not*
//! anywhere else — there is no second owner to construct.

use alloc::collections::{BTreeMap, VecDeque};

use crate::task::{ReadyTask, SchedPayload, TaskKey};

/// How the fair band decides between two ready threads of the *same* share —
/// spec §9.2's tie-break, and what simulator invariant I13 exists to hold.
///
/// The two broken orderings are reproduced for the simulator's negative gates
/// (spec §10.3) behind a feature the kernel does not enable, exactly as
/// [`crate::cpu::CpuSched::set_park_keeps_lapsed_lend`] is. A check that cannot
/// tell these apart from the shipped one is not measuring the rule it names.
#[cfg(feature = "protocol-port")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FairOrder {
    /// Spec §9.2 and the shipped code: `(vruntime, monotonic insertion
    /// sequence)`.
    #[default]
    InsertSequence,
    /// `(vruntime, TaskKey)` — the identity tie-break the field comment below
    /// warns against, ported as literally as it is written.
    IdentityTiebreak,
    /// That warning made total. Whichever share leads the band, its
    /// *lowest-keyed* ready thread is the one dispatched, whatever vruntime each
    /// of them was inserted with — so the same thread wins every time and not
    /// merely every tie. The share's pot advances at the same rate either way,
    /// so the process keeps its half of the machine and invariant I5 sees
    /// nothing at all.
    IdentityWithinShare,
}

pub struct RunQueue<X: SchedPayload> {
    rt: VecDeque<ReadyTask<X>>,
    /// Ordered by `(vruntime, insertion sequence)`.
    ///
    /// The tie-break must **not** be `TaskKey`, and the sequence must be
    /// monotonic: a re-inserted thread has to land *behind* its equal-vruntime
    /// siblings, or the same thread can win every tie and the others only run
    /// when it blocks.
    ///
    /// What that is worth *today* is smaller than it reads, and the simulator
    /// measured it rather than this comment asserting it. A share's pot is
    /// charged for every nanosecond any of its threads runs, so a thread
    /// re-inserted after a dispatch already carries a key strictly above every
    /// sibling queued before it: the band serves a share's threads in insertion
    /// order whatever the tie-break is. Exact ties survive only where no charge
    /// separates two inserts — a `wake_all` of siblings, the spawn burst — and
    /// one dispatch dissolves them. [`FairOrder::IdentityTiebreak`] is this
    /// field's warning ported literally, and it is invisible to simulator
    /// invariant I13. The rule stands because the *pot* is doing the work: a
    /// policy that stops charging it once per dispatch — an ordered map of
    /// shares each holding a FIFO of its ready threads, say — hands the whole
    /// job back here, which is what I13 is in place to guard.
    fair: BTreeMap<(u64, u64), ReadyTask<X>>,
    insert_seq: u64,
    /// Negative-gate escape hatch only; see [`FairOrder`].
    #[cfg(feature = "protocol-port")]
    order: FairOrder,
}

impl<X: SchedPayload> RunQueue<X> {
    pub fn new() -> Self {
        Self {
            rt: VecDeque::new(),
            fair: BTreeMap::new(),
            insert_seq: 0,
            #[cfg(feature = "protocol-port")]
            order: FairOrder::InsertSequence,
        }
    }

    #[cfg(feature = "protocol-port")]
    pub fn set_order(&mut self, order: FairOrder) {
        self.order = order;
    }

    /// `vruntime` orders the fair band and is ignored for RT tasks, which
    /// round-robin within their band on the same quantum.
    pub fn insert(&mut self, vruntime: u64, task: ReadyTask<X>) {
        if task.rt().is_rt() {
            self.rt.push_back(task);
        } else {
            self.insert_seq += 1;
            #[cfg(not(feature = "protocol-port"))]
            let tie = self.insert_seq;
            #[cfg(feature = "protocol-port")]
            let tie = match self.order {
                FairOrder::IdentityTiebreak => task.key().0,
                FairOrder::InsertSequence | FairOrder::IdentityWithinShare => self.insert_seq,
            };
            let previous = self.fair.insert((vruntime, tie), task);
            assert!(
                previous.is_none(),
                "two ready tasks with one (vruntime, sequence)",
            );
        }
    }

    /// RT band first, then the lowest-vruntime fair task.
    pub fn pop_next(&mut self) -> Option<(u64, ReadyTask<X>)> {
        if let Some(task) = self.rt.pop_front() {
            return Some((0, task));
        }
        #[cfg(feature = "protocol-port")]
        if self.order == FairOrder::IdentityWithinShare {
            return self.pop_lowest_key_of_leading_share();
        }
        let key = *self.fair.keys().next()?;
        let task = self.fair.remove(&key).expect("key came from the map");
        Some((key.0, task))
    }

    /// [`FairOrder::IdentityWithinShare`]: find the share that leads the band,
    /// then serve its lowest-keyed ready thread rather than its earliest-keyed
    /// one. Which share runs next is unchanged, so the per-process split is
    /// untouched; which *thread* of it runs never varies.
    #[cfg(feature = "protocol-port")]
    fn pop_lowest_key_of_leading_share(&mut self) -> Option<(u64, ReadyTask<X>)> {
        let leader = *self.fair.keys().next()?;
        let share = self.fair[&leader].share().clone();
        let chosen = *self
            .fair
            .iter()
            .filter(|(_, task)| crate::sync::Arc::ptr_eq(task.share(), &share))
            .min_by_key(|(_, task)| task.key().0)
            .expect("the leader is its own share's member")
            .0;
        let task = self.fair.remove(&chosen).expect("key came from the map");
        Some((chosen.0, task))
    }

    /// The task a [`crate::msg::Msg::StealRequest`] is answered with: the
    /// *last* fair task, i.e. the one whose turn is furthest away. Handing
    /// over the next-to-run task instead would trade a cache-warm local
    /// dispatch for a two-hop transfer.
    ///
    /// **`loaded` is skipped, and that is a correctness rule and not a policy
    /// one** — [`crate::cpu::SchedPass::answer_steal_requests`] carries the
    /// derivation. It is also the *most likely* candidate here rather than an
    /// unlikely one: a task `preempt_if_due` has just returned to the band was
    /// charged for the quantum it spent, so its vruntime is the band's highest
    /// and `next_back` names it first.
    pub fn pop_surplus(&mut self, loaded: Option<TaskKey>) -> Option<ReadyTask<X>> {
        let key = *self
            .fair
            .iter()
            .rev()
            .find(|(_, task)| Some(task.key()) != loaded)?
            .0;
        self.fair.remove(&key)
    }

    /// Retire found the task queued rather than parked.
    pub fn remove(&mut self, key: TaskKey) -> Option<ReadyTask<X>> {
        if let Some(index) = self.rt.iter().position(|t| t.key() == key) {
            return self.rt.remove(index);
        }
        let found = *self.fair.iter().find(|(_, t)| t.key() == key)?.0;
        self.fair.remove(&found)
    }

    /// Is an RT task waiting? The preemption decision in `finish()` and
    /// invariant I4's latency bound both hang off this.
    pub fn has_rt(&self) -> bool {
        !self.rt.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rt.len() + self.fair.len()
    }

    pub fn fair_len(&self) -> usize {
        self.fair.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rt.is_empty() && self.fair.is_empty()
    }

    /// Residents, for the invariant walks. Order is band-then-vruntime, i.e.
    /// pick order.
    pub fn keys(&self) -> impl Iterator<Item = TaskKey> + '_ {
        self.rt
            .iter()
            .map(|t| t.key())
            .chain(self.fair.values().map(|t| t.key()))
    }

    pub fn tasks(&self) -> impl Iterator<Item = &ReadyTask<X>> + '_ {
        self.rt.iter().chain(self.fair.values())
    }
}

impl<X: SchedPayload> Default for RunQueue<X> {
    fn default() -> Self {
        Self::new()
    }
}
