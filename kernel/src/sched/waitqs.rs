//! Where the kernel's wait *subjects* live — spec §8.6.
//!
//! **There is no wait queue in this file any more.** Every waitable
//! object owns a [`Watch`] and a waiter arms on it; the park itself is on the
//! waiter's own thread queue (`TaskHandle::park_queue`), which is the one list
//! left in the kernel and has exactly one member. Objects with a lifetime own
//! an `Arc<Watch>` (pipe ends, listeners, inbox rings), singleton devices own
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
/// **`FUTEX`, `PARK_BUCKETS` and `park_lot` are all gone from beside it**:
/// `waitpid`, `thread_join` and `nanosleep` stop hashing into a parking lot
/// and arm on the object or on their own thread, every thread parks on a
/// queue of its own
/// (`TaskHandle::park_queue`), and the futex's own 64-way queue array outlived
/// its last registrant by one chunk — `wake_n` counted an empty list and
/// `futex_wake` therefore returned 0 for every call in the machine.
pub fn futex_watch(addr: DirectMap) -> &'static Watch {
    &FUTEX_WATCH[(addr.phys() >> 2) as usize % FUTEX_BUCKETS]
}

/// End every futex wait whose word lies in `[phys, phys + len)`, because that
/// physical memory is being taken away from whoever was waiting on it.
///
/// **Called with the frame still owned**, from `AddressSpace::unmap` and under
/// the address-space lock, which is what makes it a fence rather than a race:
/// the page-table entry is already cleared, so a waiter that translated the
/// word before this ran is on the list to be found, and one that translates
/// after it finds nothing to arm on. There is no window in between for a
/// waiter to arm on a frame this call has already walked past.
///
/// **Every bucket, not the one the base hashes to.** A 2 MiB frame holds 2^19
/// words and the hash is `(phys >> 2) % 64`, so its words are spread across all
/// 64 buckets by construction — the same arithmetic that makes two words 256
/// bytes apart share one. A bucket nobody is armed on costs one relaxed load.
///
/// Without it a futex token outlives its frame: the token is a raw physical
/// address, nothing pins the frame, and the PMM hands the freshly freed one
/// straight back out — `pmm::alloc_contiguous`, which every `mmap` goes
/// through, scans the bitmap from index 0, and `pmm::free_page` lowers
/// `alloc_page`'s hint to whatever it just freed. So the next process to map
/// that frame and call `futex_wake` at the same offset wins the stale waiter's
/// claim, is counted a wake it did not make, and leaves the real waiter parked
/// for good. `futex_wake_counts`'s sweeper is that process, and on the tree
/// without this it took three of four.
pub fn revoke_futex_range(phys: u64, len: u64) -> usize {
    FUTEX_WATCH
        .iter()
        .map(|watch| completion::revoke_range(Subject::of(watch), phys, len))
        .sum()
}

