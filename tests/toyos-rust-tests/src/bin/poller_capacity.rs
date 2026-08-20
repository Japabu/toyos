//! A `Poller` cannot lose a completion inside the capacity it declared.
//!
//! `Poller::new(handles)` used to round the request up and *clamp* it, and
//! `poll_add_fd` used to flush a full submission ring mid-registration. Those
//! two together are the loss: the flush makes the kernel process registrations
//! while the caller is still registering, so handles that are already ready post
//! completions into a ring sized for a set the caller never actually declared.
//! Past `cq_size` the kernel increments `dropped` and returns, and the caller
//! blocks forever on readiness that was thrown away.
//!
//! The capacity is now a contract. Three directions, because a bound that
//! refuses everything passes the two negative cases on its own:
//!
//! 1. a poller at exactly its capacity delivers every completion;
//! 2. declaring a capacity the kernel cannot build is refused, not clamped;
//! 3. registering past the declared capacity is refused, not flushed.

use std::process::Command;

use toyos::poller::{Poller, IORING_POLL_IN};
use toyos_abi::syscall;

const SELF_PATH: &str = "/bin/test_rs_poller_capacity";

/// Each pipe is one 2 MiB kernel page, so this is 128 MiB of ring — enough to
/// be a real batch, small enough to leave the machine alone.
const CAP: u32 = 64;

/// Printed by a child immediately before the call that must not return.
const ABOUT_TO: &str = "about to violate the contract";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("over-capacity") => return over_capacity(),
        Some("over-register") => return over_register(),
        _ => {}
    }

    full_capacity_delivers_everything();
    expect_child_death("over-capacity");
    expect_child_death("over-register");

    println!("poller capacity honoured: {CAP} handles delivered, both violations refused");
}

/// Every handle in a full batch completes, and nothing is dropped.
///
/// The `dropped` assert inside `wait` is the other half of this: it reads the
/// kernel's counter, so a completion thrown away fails here rather than
/// silently shortening the token set.
fn full_capacity_delivers_everything() {
    let mut pipes = Vec::new();
    for _ in 0..CAP {
        let p = syscall::pipe().expect("a pipe to poll");
        // Readable before it is ever registered, which is the case that used
        // to post a completion mid-registration.
        syscall::write(p.write, b"x").expect("write into a fresh pipe");
        pipes.push(p);
    }

    let poller = Poller::new(CAP);
    for (i, p) in pipes.iter().enumerate() {
        poller.poll_add_fd(p.read, IORING_POLL_IN, i as u64);
    }

    let mut seen = vec![false; CAP as usize];
    poller.wait(CAP, 1_000_000_000, |token| {
        assert!((token as usize) < CAP as usize, "unknown token {token}");
        seen[token as usize] = true;
    });

    let missing: Vec<usize> = seen.iter().enumerate().filter(|(_, s)| !**s).map(|(i, _)| i).collect();
    assert!(
        missing.is_empty(),
        "{} of {CAP} ready handles never completed: {missing:?}",
        missing.len()
    );

    for p in pipes {
        syscall::close(p.read);
        syscall::close(p.write);
    }
}

/// A capacity above what the kernel will build must be refused. Clamping it
/// hands back a ring smaller than the set the caller just declared, which is
/// exactly the state in which the loss is reachable and looks like success.
fn over_capacity() {
    println!("{ABOUT_TO}");
    let _ = Poller::new(Poller::MAX_HANDLES + 1);
    println!("Poller::new accepted {} handles", Poller::MAX_HANDLES + 1);
}

/// Registering past the declared capacity must be refused. It used to flush
/// and carry on, which is where the dropped completions came from.
fn over_register() {
    const SMALL: u32 = 4;
    let poller = Poller::new(SMALL);
    let mut pipes = Vec::new();
    for _ in 0..=SMALL {
        let p = syscall::pipe().expect("a pipe to poll");
        syscall::write(p.write, b"x").expect("write into a fresh pipe");
        pipes.push(p);
    }
    for (i, p) in pipes.iter().enumerate().take(SMALL as usize) {
        poller.poll_add_fd(p.read, IORING_POLL_IN, i as u64);
    }
    println!("{ABOUT_TO}");
    poller.poll_add_fd(pipes[SMALL as usize].read, IORING_POLL_IN, SMALL as u64);
    println!("poll_add_fd accepted registration {} of a {SMALL}-handle poller", SMALL + 1);
}

/// Run this binary again with `arg` and require that it died at the contract
/// violation — not before it, which a plain "exited nonzero" would also accept.
fn expect_child_death(arg: &str) {
    let out = Command::new(SELF_PATH).arg(arg).output().expect("spawn child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(ABOUT_TO),
        "the {arg} child died before reaching the violation; stdout: {stdout:?}"
    );
    assert!(
        !out.status.success(),
        "the {arg} child survived the violation; stdout: {stdout:?}"
    );
}
