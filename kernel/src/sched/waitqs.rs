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

/// Parking lots for waits that are woken by name rather than by condition:
/// `waitpid`, `thread_join`, `nanosleep`. See the module note on why these are
/// never woken as queues.
const PARK_BUCKETS: usize = 32;
static PARK: [KWaitQueue; PARK_BUCKETS] = [const { static_queue(WaitClass::Other) }; PARK_BUCKETS];

pub static KEYBOARD: KWaitQueue = static_queue(WaitClass::Io);
pub static MOUSE: KWaitQueue = static_queue(WaitClass::Io);
pub static NETWORK: KWaitQueue = static_queue(WaitClass::Io);
pub static AUDIO: KWaitQueue = static_queue(WaitClass::Io);

/// The completion watch beside each device queue
/// (`specs/completion-architecture-spec.md` §5.2). One object, two ways of
/// waiting on it, until C3 deletes the queue half.
pub static KEYBOARD_WATCH: Watch = Watch::new();
pub static MOUSE_WATCH: Watch = Watch::new();
pub static NETWORK_WATCH: Watch = Watch::new();
pub static AUDIO_WATCH: Watch = Watch::new();

/// Wake a device queue **and** post to its watch.
///
/// One function rather than a pair at each site, and that is the whole point:
/// `complete_pending_for_event` has ten hand-paired call sites and
/// `io-uring-source-half-a-wake-pair` records losing that pairing twice in one
/// cutover (§5.6). Here the pairing is a call, so half of it cannot be
/// forgotten.
pub fn wake_device(queue: &KWaitQueue, watch: &'static Watch) -> usize {
    let woken = wake_all(queue);
    completion::post(Subject::of(watch), Outcome::Ready);
    woken
}

/// The bucket a futex word parks in, keyed by physical address so the queue is
/// shared across every process that maps it.
pub fn futex(addr: DirectMap) -> &'static KWaitQueue {
    &FUTEX[(addr.phys() >> 2) as usize % FUTEX_BUCKETS]
}

/// A parking lot for the running thread. Any bucket would do; hashing spreads
/// the `Registration::finish` scan.
pub fn park_lot(seed: u64) -> &'static KWaitQueue {
    &PARK[seed as usize % PARK_BUCKETS]
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

/// Wake every waiter and lend each an RT window until `until` (spec §8.5).
pub fn wake_all_boosted(queue: &KWaitQueue, until: toyos_sched::hw::Nanos) -> usize {
    preempt_off(|p| {
        queue.wake_all(
            WakeCause::boosted(WakeReason::Woken, until),
            cpus(),
            &HW,
            p,
        )
    })
}
