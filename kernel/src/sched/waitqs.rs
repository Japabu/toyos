//! Where the kernel's wait queues live — spec §8.6.
//!
//! Every waitable object owns its queue. Objects with a lifetime own an
//! `Arc<KWaitQueue>` (pipe ends, listeners, io_uring rings); objects that are
//! singletons own a `static` (the devices); and the two sets with no object at
//! all — futex words and the by-name wakes of join/waitpid/sleep — are hashed
//! into fixed bucket arrays, because a bucket is a *place to park*, not a set
//! whose membership means anything.
//!
//! The two bucket arrays differ in one important way. A futex bucket is woken
//! with `wake_one`/`wake_all`, so sharing a bucket costs a spurious wake and
//! nothing else (every blocking site loops). A park bucket is **never** woken
//! as a queue: `wake_direct` claims the task's own rendezvous word and the
//! queue node is cleaned up by the waiter's own `Registration`. Waking a park
//! bucket would satisfy a wake with an unrelated sleeper, so nothing does.

use alloc::sync::Arc;

use toyos_sched::task::{WaitClass, WakeCause, WakeReason};

use crate::completion::{self, Outcome, Subject, Watch};
use crate::hw::HW;
use crate::DirectMap;

use super::driver::{cpus, preempt_off};
use super::payload::{static_queue, KWaitQueue};

/// Enough that two live futex words rarely share one, small enough to sit in
/// `.bss`. A collision costs a spurious wake; all waiters re-check their word.
const FUTEX_BUCKETS: usize = 64;
static FUTEX: [KWaitQueue; FUTEX_BUCKETS] =
    [const { static_queue(WaitClass::Futex) }; FUTEX_BUCKETS];
static FUTEX_WATCH: [Watch; FUTEX_BUCKETS] = [const { Watch::new() }; FUTEX_BUCKETS];

/// The device subjects. **The `KWaitQueue`s that used to stand beside these
/// are gone**: after §5.6 a reader arms here and parks on its own thread's
/// queue, so a shared list per device had nothing left in it.
pub static KEYBOARD_WATCH: Watch = Watch::new();
pub static MOUSE_WATCH: Watch = Watch::new();
pub static NETWORK_WATCH: Watch = Watch::new();
pub static AUDIO_WATCH: Watch = Watch::new();

/// Tell a device's waiters that it has something.
///
/// **One call where there was a pair.** `complete_pending_for_event` has ten
/// hand-paired call sites and `io-uring-source-half-a-wake-pair` records
/// losing that pairing twice in one cutover (§5.6); the queue half is gone
/// now, and what is left is the post.
pub fn wake_device(watch: &'static Watch) {
    completion::post(Subject::of(watch), Outcome::Ready);
}

/// The bucket a futex word parks in, keyed by physical address so the queue is
/// shared across every process that maps it.
pub fn futex(addr: DirectMap) -> &'static KWaitQueue {
    &FUTEX[(addr.phys() >> 2) as usize % FUTEX_BUCKETS]
}

/// The completion subject a futex word parks on, keyed the same way.
///
/// **`PARK_BUCKETS` and `park_lot` are gone from beside it**
/// (`specs/completion-architecture-spec.md` §5.6): `waitpid`, `thread_join`
/// and `nanosleep` stop hashing into a parking lot and arm on the object or on
/// their own thread, and every thread now parks on a queue of its own
/// (`TaskHandle::park_queue`).
pub fn futex_watch(addr: DirectMap) -> &'static Watch {
    &FUTEX_WATCH[(addr.phys() >> 2) as usize % FUTEX_BUCKETS]
}

pub fn new_queue(class: WaitClass) -> Arc<KWaitQueue> {
    Arc::new(static_queue(class))
}

pub fn wake_n(queue: &KWaitQueue, count: usize) -> usize {
    preempt_off(|p| {
        let mut woken = 0;
        while woken < count {
            if queue.wake_one(WakeCause::new(WakeReason::Woken), cpus(), &HW, p) == 0 {
                break;
            }
            woken += 1;
        }
        woken
    })
}

pub fn wake_all(queue: &KWaitQueue) -> usize {
    preempt_off(|p| {
        queue.wake_all(WakeCause::new(WakeReason::Woken), cpus(), &HW, p)
    })
}


