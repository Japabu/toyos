//! Where the kernel's wait *subjects* live — spec §8.6.
//!
//! **There is no wait queue in this file any more, and that is the whole of
//! what `specs/completion-architecture-spec.md` §5.6 asked for.** Every waitable
//! object owns a [`Watch`] and a waiter arms on it; the park itself is on the
//! waiter's own thread queue (`TaskHandle::park_queue`), which is the one list
//! left in the kernel and has exactly one member. Objects with a lifetime own
//! an `Arc<Watch>` (pipe ends, listeners, io_uring rings), singleton devices own
//! a `static`, and futex words — which have no object at all — hash into a fixed
//! bucket array, because a bucket is a *place to arm*, not a set whose
//! membership means anything.
//!
//! **A shared bucket is not a shared wake.** A watcher carries the token it
//! armed with, and a futex waiter's token is its word's physical address, so
//! `completion::post_n` walks the bucket and names the word. Sharing a bucket
//! therefore costs a list walk and not a spurious wake — which is what makes
//! `SYS_FUTEX_WAKE`'s count and its return value mean anything.

use crate::completion::{self, Outcome, Subject, Watch};
use crate::DirectMap;

/// Enough that two live futex words rarely share one, small enough to sit in
/// `.bss`. A collision costs a longer walk and nothing else: the walk matches
/// on the waiter's token, which is the word.
const FUTEX_BUCKETS: usize = 64;
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

/// The completion subject a futex word arms on, keyed by physical address so
/// the subject is shared across every process that maps the word.
///
/// **`FUTEX`, `PARK_BUCKETS` and `park_lot` are all gone from beside it**
/// (`specs/completion-architecture-spec.md` §5.6): `waitpid`, `thread_join` and
/// `nanosleep` stop hashing into a parking lot and arm on the object or on
/// their own thread, every thread parks on a queue of its own
/// (`TaskHandle::park_queue`), and the futex's own 64-way queue array outlived
/// its last registrant by one chunk — `wake_n` counted an empty list and
/// `futex_wake` therefore returned 0 for every call in the machine.
pub fn futex_watch(addr: DirectMap) -> &'static Watch {
    &FUTEX_WATCH[(addr.phys() >> 2) as usize % FUTEX_BUCKETS]
}

