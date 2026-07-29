//! The per-CPU run queue — spec §9.2.
//!
//! Two bands, deliberately today's ordering: an RT FIFO drained first, and a
//! fair band ordered by `(vruntime, insertion sequence)`. The stored-lag fairness
//! semantics are preserved bit-identically through the machinery cutover so
//! that any regression is attributable to the machinery; true EEVDF
//! virtual-deadline ordering is a later, sim-gated, `queue.rs`/`fair.rs`-only
//! change (spec §9.1).
//!
//! The queue owns [`ReadyTask`] values. A task in a queue is therefore *not*
//! anywhere else — there is no second owner to construct.

use alloc::collections::{BTreeMap, VecDeque};

use crate::task::{ReadyTask, SchedPayload, TaskKey};

pub struct RunQueue<X: SchedPayload> {
    rt: VecDeque<ReadyTask<X>>,
    /// Ordered by `(vruntime, insertion sequence)`.
    ///
    /// The tie-break is deliberately **not** `TaskKey`. All threads of a
    /// process share one vruntime, so an identity tie-break is deterministic:
    /// the same thread wins every tie and its siblings only run when it blocks.
    /// That is not hypothetical — it starved Doom's midi thread behind its game
    /// thread on a single core, and the old scheduler carries the same
    /// monotonic-sequence fix for the same reason. A re-inserted thread goes
    /// *behind* its equal-vruntime siblings, so threads of one process
    /// round-robin without gaining any cross-process share.
    fair: BTreeMap<(u64, u64), ReadyTask<X>>,
    insert_seq: u64,
}

impl<X: SchedPayload> RunQueue<X> {
    pub fn new() -> Self {
        Self {
            rt: VecDeque::new(),
            fair: BTreeMap::new(),
            insert_seq: 0,
        }
    }

    /// `vruntime` orders the fair band and is ignored for RT tasks, which
    /// round-robin within their band on the same quantum.
    pub fn insert(&mut self, vruntime: u64, task: ReadyTask<X>) {
        if task.rt().is_rt() {
            self.rt.push_back(task);
        } else {
            self.insert_seq += 1;
            let previous = self.fair.insert((vruntime, self.insert_seq), task);
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
        let key = *self.fair.keys().next()?;
        let task = self.fair.remove(&key).expect("key came from the map");
        Some((key.0, task))
    }

    /// The task a [`crate::msg::Msg::StealRequest`] is answered with: the
    /// *last* fair task, i.e. the one whose turn is furthest away. Handing
    /// over the next-to-run task instead would trade a cache-warm local
    /// dispatch for a two-hop transfer.
    pub fn pop_surplus(&mut self) -> Option<ReadyTask<X>> {
        let key = *self.fair.keys().next_back()?;
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
