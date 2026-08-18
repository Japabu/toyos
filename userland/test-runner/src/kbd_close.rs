//! A pending poll on stdin outlives the keyboard *claim* being closed.
//!
//! **The defect this is aimed at was cancellation by an object that did not own
//! the source.** `io_uring::remove_fd` cancels by source across every ring in
//! the machine — right for a pipe, whose other end really has gone — and
//! `object::ops::close` decided whether to call it by asking the *object*:
//! `Device(_)` answered "this ends its sources", on the argument that a claim
//! admits exactly one handle so every ring watching it is the one holder's.
//! That is true of the claim and false of the source. `Source::Keyboard` is
//! named by the `Device(Keyboard)` claim *and* by every `Console`
//! (`object::ops::read_source`), so the claim's holder closing its handle posted
//! `-NotFound` into every pending `POLL_ADD` on stdin in the machine — libc's
//! terminal read is what arms them — for processes that hold no device and were
//! never consulted.
//!
//! It runs inside `test-runner` because a spawned binary's stdin is a pipe: this
//! process's fd 0 is the `Console` that names `Source::Keyboard`, and the claim
//! it closes is minted from the capability the estate holds. Both objects are in
//! one process, which is the smallest machine the collision exists on; the real
//! failure needs two, and neither has to know about the other.
//!
//! Three arms, and the middle one is why it is not enough to assert that the
//! poll survived:
//!
//! 1. the keyboard claim is taken and released, and the stdin poll must still be
//!    pending — the arm that reds on the defect;
//! 2. the **mouse** claim is polled and released, and *that* poll must be
//!    cancelled — `Source::Mouse` is named by nothing but the claim, so
//!    cancellation is exactly what a close there owes. This is the direction a
//!    fix overshoots into: a tree that stopped cancelling on close would pass
//!    arm 1 and red here;
//! 3. an injected keystroke completes the stdin poll — so what survived arm 1
//!    was a live registration and not an absent one.

use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::syscap::SysCap;
use toyos::{Keyboard, Mouse};
use toyos_abi::syscall::DeviceType;
use toyos_abi::RawHandle;

/// This process's console. `object::ops::read_source` maps every `Console` to
/// `Source::Keyboard`, which is the whole of the collision under test.
const STDIN: RawHandle = RawHandle(0);

const STDIN_TOKEN: u64 = 1;
const MOUSE_TOKEN: u64 = 2;

/// What the host waits for before it injects. Printed only once both claim arms
/// have run, so a key can never arrive early enough to complete the poll the
/// first arm is asserting is still pending.
const READY: &str = "===KBD_CLOSE_READY===";

/// How long arm 3 gives an injected keystroke.
///
/// A liveness bound and not a verdict: what has to happen is one key transition
/// reaching `keyboard::handle_key`, which posts to this ring's watcher list
/// before the interrupt returns. Seconds is four orders of magnitude above it,
/// and the host injects only after [`READY`].
const KEY_WAIT_NANOS: u64 = 5_000_000_000;

/// Completions seen so far, by token. Accumulated across drains because a drain
/// consumes the ring: asking twice about the same completion is asking about
/// nothing.
#[derive(Default)]
struct Seen {
    stdin: usize,
    mouse: usize,
}

pub fn run(cap: Option<&SysCap>) -> i32 {
    let Some(cap) = cap else {
        println!("kbd-close: this program holds no system capability, so it can mint no claim");
        return 1;
    };
    match probe(cap) {
        Ok(()) => {
            println!("kbd-close: OK");
            0
        }
        Err(e) => {
            println!("kbd-close: FAILED: {e}");
            1
        }
    }
}

fn probe(cap: &SysCap) -> Result<(), String> {
    let poller = Poller::new(2);
    let mut seen = Seen::default();

    // **Submitted before anything is closed, and that is the whole of what this
    // has to get right.** `poll_add_fd` only queues a submission entry; `wait`
    // is what enters the kernel. A probe that closed first would stage nothing
    // — the ring is not a watcher of the keyboard yet, so there is nothing for
    // a cancellation to reach, and it would pass on a tree with the defect.
    poller.poll_add_fd(STDIN, IORING_POLL_IN, STDIN_TOKEN);
    drain(&poller, &mut seen);
    if poller.pending() != 0 {
        return Err(format!("{} submission(s) never reached the kernel", poller.pending()));
    }
    if seen.stdin != 0 {
        return Err(
            "the console was already readable, so nothing here would have been pending; this \
             guest was given input before the gate ran"
                .to_string(),
        );
    }

    // Arm 2 first, so a tree that has stopped cancelling anything is caught
    // before arm 1 congratulates it. The mouse is the device every machine
    // shape has, and `Source::Mouse` is named by its claim and by nothing else.
    let mouse: Mouse = cap
        .claim(DeviceType::Mouse)
        .map_err(|e| format!("the mouse must be claimable and answered {e:?}"))?;
    poller.poll_add(&mouse, IORING_POLL_IN, MOUSE_TOKEN);
    drain(&poller, &mut seen);
    if seen.mouse != 0 {
        return Err("the mouse reported input; this gate needs an idle pointer".to_string());
    }
    drop(mouse);
    // Synchronous: `ops::close` posts the cancellation before the `close`
    // syscall returns, on this thread.
    drain(&poller, &mut seen);
    if seen.mouse != 1 {
        return Err(format!(
            "releasing the mouse claim left its own poll pending ({} completions) — a source \
             that no other kind of object names must be cancelled by its holder's close",
            seen.mouse,
        ));
    }
    if seen.stdin != 0 {
        return Err("closing the mouse claim completed the poll on stdin".to_string());
    }

    // Arm 1. Nothing about the machine's keyboard changes here: the class is
    // claimed and released, and the console still names the same source.
    let keyboard: Keyboard = cap
        .claim(DeviceType::Keyboard)
        .map_err(|e| format!("the keyboard must be claimable and answered {e:?}"))?;
    drop(keyboard);
    drain(&poller, &mut seen);
    if seen.stdin != 0 {
        return Err(
            "releasing the keyboard claim cancelled a poll on stdin — a process that holds no \
             device had its terminal read completed from under it"
                .to_string(),
        );
    }

    // Arm 3.
    println!("{READY}");
    poller.wait(1, KEY_WAIT_NANOS, |token| count(&mut seen, token));
    if seen.stdin == 0 {
        return Err(
            "the poll outlived the close and then never completed on a keystroke either, so \
             what it outlived may have been its own arming"
                .to_string(),
        );
    }
    println!("kbd-close: survived=1 mouse_cancelled={} stdin_woken={}", seen.mouse, seen.stdin);
    Ok(())
}

/// Take whatever is in the completion ring right now, without waiting.
fn drain(poller: &Poller, seen: &mut Seen) {
    poller.wait(1, 0, |token| count(seen, token));
}

fn count(seen: &mut Seen, token: u64) {
    match token {
        STDIN_TOKEN => seen.stdin += 1,
        MOUSE_TOKEN => seen.mouse += 1,
        other => panic!("kbd-close: a completion for a token nothing submitted: {other}"),
    }
}
