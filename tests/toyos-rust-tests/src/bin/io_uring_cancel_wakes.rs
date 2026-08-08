//! A cancelled `POLL_ADD` must wake the thread that is waiting for it.
//!
//! `io_uring::remove_fd` cancels every pending poll on a source that is going
//! away and posts `-NotFound` for each, so the caller knows to look at the
//! handle again. It posted them into the ring and woke nobody — and nothing
//! else can end that wait: the poll is gone, so the source's own wake path
//! finds no watcher for it, and a `u64::MAX` wait therefore never returns.
//! Every server in the tree waits that way.
//!
//! Two descriptors on one pipe, which is what an ordinary `dup`/`dup2` of
//! stdio leaves behind, and closing one of them is the whole stimulus. The
//! pipe keeps a reader either way, so `close_read`'s own wake path is not
//! involved and cannot mask the missing one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use toyos::poller::{Poller, IORING_POLL_IN};
use toyos_abi::syscall;

const TOKEN: u64 = 7;

/// How long the waiter's own `wait` may take before this is a hang and not a
/// slow guest. The cancellation is posted the instant `close` runs, so a
/// healthy kernel returns in microseconds; seconds of margin cost nothing.
const PATIENCE: Duration = Duration::from_secs(10);

/// Between the waiter saying it is about to park and the close that cancels
/// it. Only the ordering matters, and it is benign in both directions — a
/// close that lands before the park leaves the completion sitting in the ring
/// and the wait returns at once — but the defect is only visible with the
/// thread genuinely parked, so this is wide.
const PARK_MARGIN: Duration = Duration::from_millis(500);

static REGISTERED: AtomicBool = AtomicBool::new(false);
static RETURNED: AtomicBool = AtomicBool::new(false);

fn main() {
    let pipe = syscall::pipe();
    // The second descriptor. Closing this one is what `remove_fd` acts on,
    // while `pipe.read` keeps the pipe's reader count above zero.
    let dup = syscall::dup(pipe.read).expect("dup the read end");

    let waiter = thread::spawn(move || {
        let poller = Poller::new(4);
        poller.poll_add_fd(pipe.read, IORING_POLL_IN, TOKEN);
        // A non-blocking enter, so the poll is registered in the kernel before
        // anything is closed. Without it the close could reach a ring with
        // nothing pending in it and cancel nothing at all, which is a
        // different test that would pass on a broken kernel.
        poller.wait(0, 0, |token| panic!("nothing is ready yet, got token {token}"));

        REGISTERED.store(true, Ordering::Release);
        let start = Instant::now();
        let mut tokens = Vec::new();
        poller.wait(1, u64::MAX, |token| tokens.push(token));
        RETURNED.store(true, Ordering::Release);
        (start.elapsed(), tokens)
    });

    while !REGISTERED.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(PARK_MARGIN);
    syscall::close(dup);

    let deadline = Instant::now() + PATIENCE;
    while !RETURNED.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if !RETURNED.load(Ordering::Acquire) {
        // Nothing can release the waiter now: its poll was cancelled, so the
        // watcher list no longer names its ring and a write to the pipe would
        // complete nothing. Report and take the process down rather than hand
        // the harness a timeout with no message in it.
        println!(
            "the waiter is still parked {PATIENCE:?} after its poll was cancelled — \
             the cancellation posted a completion and woke nobody"
        );
        std::process::exit(1);
    }

    let (took, tokens) = waiter.join().expect("the waiter thread panicked");
    assert_eq!(tokens, [TOKEN], "the wait returned with the wrong completions");
    println!("a cancelled poll woke its waiter in {took:.2?}");

    syscall::close(pipe.read);
    syscall::close(pipe.write);
}
