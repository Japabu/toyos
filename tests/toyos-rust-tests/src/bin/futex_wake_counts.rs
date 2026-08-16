//! `SYS_FUTEX_WAKE` wakes **up to `count`** waiters on **this word**, and
//! answers how many.
//!
//! Both halves are the ABI's own sentence (`toyos-abi/src/syscall.rs`: "wake up
//! to `count` threads waiting on `addr`. Returns number of threads woken"), and
//! the completion cutover briefly honoured neither. The count went to a 64-way
//! bucket queue that nothing had registered on since the park moved to the
//! thread's own queue — so the return was **provably always 0**, for every call
//! in the machine — and the wake that actually happened was an uncounted post to
//! every waiter on the shared bucket, which turns `pthread_cond_signal` into a
//! broadcast and can spend one thread's wake on a waiter of a different word.
//!
//! Nothing in the tree noticed, because nothing in the tree asks: `libc`'s
//! `pthread` discards the return, and the std fork's `RwLock::wake_writer`
//! reads a permanently-false answer as documented-but-pessimal. So this asks.
//!
//! **The two words are 256 bytes apart on purpose.** The bucket is
//! `(phys >> 2) % 64`, so words 256 bytes apart in one page land in the *same*
//! bucket by construction — which is the only way to test that a shared bucket
//! is not a shared wake. A page-aligned static is what makes their physical
//! offset equal their virtual one.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use toyos_abi::syscall;

/// Two futex words in one page, `FUTEX_BUCKETS * 4` bytes apart, so
/// `(phys >> 2) % 64` is the same for both.
#[repr(C, align(4096))]
struct SameBucket {
    word: AtomicU32,
    _pad: [u8; 256 - 4],
    sibling: AtomicU32,
}

static WORDS: SameBucket = SameBucket {
    word: AtomicU32::new(0),
    _pad: [0; 256 - 4],
    sibling: AtomicU32::new(0),
};

/// How long the waiters are given to reach their `futex_wait` before the first
/// wake. **A margin and not a bound**: every assertion below is about a
/// *number returned*, so a waiter that had not parked yet makes this test
/// weaker rather than wrong — it would count one fewer, and the count-limit
/// assertions would fail loudly rather than pass vacuously.
const PARK_MARGIN: Duration = Duration::from_millis(300);

static WORD_RETURNED: AtomicU32 = AtomicU32::new(0);
static SIBLING_RETURNED: AtomicU32 = AtomicU32::new(0);

fn main() {
    let waiters: Vec<_> = (0..2)
        .map(|_| {
            thread::spawn(|| {
                // Returns only once the word has actually changed: the kernel's
                // `futex_wait` re-reads it after every wake, which is what makes
                // "was this thread told" observable at all.
                unsafe { syscall::futex_wait(WORDS.word.as_ptr(), 0, None) };
                WORD_RETURNED.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect();
    let sibling = thread::spawn(|| {
        unsafe { syscall::futex_wait(WORDS.sibling.as_ptr(), 0, None) };
        SIBLING_RETURNED.fetch_add(1, Ordering::SeqCst);
    });
    thread::sleep(PARK_MARGIN);

    // The word changes first, so a waiter that is told goes home instead of
    // re-parking — otherwise it would re-arm and be counted twice.
    WORDS.word.store(1, Ordering::SeqCst);

    let one = unsafe { syscall::futex_wake(WORDS.word.as_ptr(), 1) };
    assert_eq!(
        one, 1,
        "futex_wake(count=1) with two waiters answered {one}, and the ABI's answer is \
         the number of threads woken",
    );
    thread::sleep(PARK_MARGIN);
    let returned = WORD_RETURNED.load(Ordering::SeqCst);
    assert_eq!(
        returned, 1,
        "futex_wake(count=1) woke {returned} of two waiters — a count-limited wake is what \
         makes pthread_cond_signal a signal rather than a broadcast",
    );
    let leaked = SIBLING_RETURNED.load(Ordering::SeqCst);
    assert_eq!(
        leaked, 0,
        "waking one word woke a waiter of the other word in the same bucket — a shared \
         bucket is a place to arm, not a set of threads to wake",
    );

    let rest = unsafe { syscall::futex_wake(WORDS.word.as_ptr(), 10) };
    assert_eq!(rest, 1, "one waiter was left on this word, and futex_wake answered {rest}");
    let none = unsafe { syscall::futex_wake(WORDS.word.as_ptr(), 10) };
    assert_eq!(none, 0, "nobody is left on this word, and futex_wake answered {none}");

    WORDS.sibling.store(1, Ordering::SeqCst);
    let other = unsafe { syscall::futex_wake(WORDS.sibling.as_ptr(), 10) };
    assert_eq!(
        other, 1,
        "the other word's waiter was still parked and answerable after three wakes of its \
         bucket-mate, and futex_wake answered {other}",
    );

    for waiter in waiters {
        waiter.join().expect("a word waiter panicked");
    }
    sibling.join().expect("the sibling waiter panicked");
    println!("futex_wake respects its count, names its word, and says how many it woke");
}
